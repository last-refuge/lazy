//! Attachments: uploading files, attaching them to messages, serving them, and cleaning up
//! (llm-docs/DECISIONS.md, D17).
//!
//! # The life of an attachment
//!
//! 1. **Upload** (`PUT /v1/conversations/{c}/attachments/{id}`). The client chooses the id. The
//!    body is streamed to a staging file, hashed on the way ([`store::AttachmentStore::stage`]);
//!    an image is decoded for its size and a thumbnail on the blocking pool; then, under a
//!    per-id advisory lock, the file is committed under its name, the thumbnail stored, and the
//!    row inserted. A crash can therefore leave a file with no row (the daily sweep removes it
//!    once it is old enough that no upload can still be about to write its row), but never a row
//!    with no file. The attachment is *pending*: only its uploader can see it.
//! 2. **Attach.** A send naming the id (`attachment_ids`) checks, in the message's own
//!    transaction, that each is the sender's, for this conversation, pending, and unattached, and
//!    attaches them in order ([`claim_for_send`], [`attach`]).
//! 3. **Serve** (`GET /v1/attachments/{id}`, `/thumbnail`) to whoever may read the conversation;
//!    a deleted message's files are gone at once.
//! 4. **Clean up.** A message's deletion marks its attachments deleted in the same transaction
//!    (access ends there); the hourly job removes the files of rows marked deleted, and of
//!    pending uploads past their time, then the rows ([`cleanup`]).
//!
//! Locks: the upload takes `attachment-user:{uploader}` then `attachment:{id}` (advisory, in that
//! order, in its last transaction); a send locks the attachment rows it names, in id order, after
//! the conversation and the thread's root and before the workspace row.

pub mod images;
pub mod store;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE,
    ETAG, IF_NONE_MATCH, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use lazy_domain::attachment::{
    MAX_ATTACHMENTS_PER_MESSAGE, is_previewable, resolve_media_type, sanitize_file_name,
};
use lazy_domain::{AttachmentId, ConversationId, UserId};
use lazy_protocol::{Attachment, ErrorCode};
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use tokio::sync::Semaphore;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::app::AppState;
use crate::config::{AttachmentBackend, AttachmentOptions};
use crate::conversations;
use crate::error::AppError;
use crate::logging::Describe;
use store::{
    AttachmentStore, EntryKind, FileStore, MemoryStore, ObjectKey, ObjectKind, StoreError,
};

/// Pending uploads one person may hold: how many files, and how many bytes in all.
pub const MAX_PENDING_FILES: i64 = 50;
pub const MAX_PENDING_BYTES: i64 = 200 * 1024 * 1024;

/// What the server keeps for attachments, shared by every request.
pub struct Attachments {
    pub store: Arc<dyn AttachmentStore>,
    pub options: AttachmentOptions,
    /// Limits the images decoded at once.
    pub(crate) thumbnail_slots: Semaphore,
}

impl Attachments {
    /// Opens the backend the options name.
    pub fn open(options: AttachmentOptions) -> Result<Self, AppError> {
        let store: Arc<dyn AttachmentStore> = match &options.backend {
            AttachmentBackend::Memory => Arc::new(MemoryStore::new()),
            AttachmentBackend::Directory(dir) => {
                Arc::new(FileStore::open_dir(dir).map_err(|e| {
                    AppError::internal(format!("cannot use the attachments directory: {e}"))
                })?)
            }
        };
        Ok(Self::with_store(store, options))
    }

    pub fn with_store(store: Arc<dyn AttachmentStore>, options: AttachmentOptions) -> Self {
        let slots = options.thumbnail_slots.max(1);
        Self {
            store,
            options,
            thumbnail_slots: Semaphore::new(slots),
        }
    }
}

fn not_found() -> AppError {
    AppError::not_found("attachment not found")
}

/// A request body as a stream the store can stage. A body that fails (the client went away) is
/// reported as a connection that was aborted.
pub(crate) fn body_stream(
    body: Body,
) -> futures_util::stream::BoxStream<'static, std::io::Result<bytes::Bytes>> {
    body.into_data_stream()
        .map(|chunk| {
            chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e))
        })
        .boxed()
}

// --- Rows ------------------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct AttachmentRow {
    id: Uuid,
    conversation_id: Uuid,
    uploader_id: Uuid,
    name: String,
    media_type: String,
    size: i64,
    sha256: Vec<u8>,
    width: Option<i32>,
    height: Option<i32>,
    has_thumbnail: bool,
    thumbnail_type: Option<String>,
    message_id: Option<Uuid>,
    deleted_at: Option<OffsetDateTime>,
}

macro_rules! attachment_select {
    ($tail:literal) => {
        concat!(
            "SELECT id, conversation_id, uploader_id, name, media_type, size, sha256, width, height, \
                    has_thumbnail, thumbnail_type, message_id, deleted_at FROM attachments ",
            $tail
        )
    };
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

impl AttachmentRow {
    fn to_attachment(&self) -> Result<Attachment, AppError> {
        Ok(Attachment {
            id: AttachmentId::from_uuid(self.id),
            name: self.name.clone(),
            media_type: self.media_type.clone(),
            size: u64::try_from(self.size)
                .map_err(|_| AppError::internal("stored attachment size is negative"))?,
            sha256: hex(&self.sha256),
            width: self.width.and_then(|w| u32::try_from(w).ok()),
            height: self.height.and_then(|h| u32::try_from(h).ok()),
            has_thumbnail: self.has_thumbnail,
        })
    }
}

async fn find(conn: &mut PgConnection, id: Uuid) -> Result<Option<AttachmentRow>, AppError> {
    Ok(sqlx::query_as(attachment_select!("WHERE id = $1"))
        .bind(id)
        .fetch_optional(conn)
        .await?)
}

// --- Uploading -------------------------------------------------------------------------------

/// The file name in the `x-attachment-name` header: percent-encoded UTF-8, made safe. A missing
/// header is `file`; text that is not valid percent-encoded UTF-8 is `invalid_request`.
fn name_from(headers: &HeaderMap) -> Result<String, AppError> {
    let Some(value) = headers.get(lazy_protocol::api::paths::ATTACHMENT_NAME_HEADER) else {
        return Ok("file".to_owned());
    };
    let text = value
        .to_str()
        .map_err(|_| AppError::invalid("the file name must be percent-encoded"))?;
    let decoded = percent_decode(text)
        .ok_or_else(|| AppError::invalid("the file name is not valid percent-encoded UTF-8"))?;
    Ok(sanitize_file_name(&decoded))
}

/// `%XX` escapes decoded; `None` for a bad escape or bytes that are not UTF-8. (`+` is a plus.)
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let pair = bytes.get(i + 1..i + 3)?;
            let digit = |b: u8| (b as char).to_digit(16);
            out.push((digit(pair[0])? * 16 + digit(pair[1])?) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

pub(crate) fn declared_length(headers: &HeaderMap, max: u64) -> Result<u64, AppError> {
    let length = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| AppError::invalid("Content-Length is required for an upload"))?;
    if length > max {
        return Err(AppError::new(
            ErrorCode::PayloadTooLarge,
            format!("a file can be at most {max} bytes"),
        ));
    }
    Ok(length)
}

/// What one person has waiting: files and bytes of uploads no message names yet.
async fn pending_of(conn: &mut PgConnection, user: Uuid) -> Result<(i64, i64), AppError> {
    Ok(sqlx::query_as(
        "SELECT count(*), COALESCE(sum(size), 0)::bigint FROM attachments \
         WHERE uploader_id = $1 AND message_id IS NULL AND deleted_at IS NULL",
    )
    .bind(user)
    .fetch_one(conn)
    .await?)
}

fn too_much_pending() -> AppError {
    AppError::new(
        ErrorCode::LimitReached,
        format!(
            "at most {MAX_PENDING_FILES} files or {} MiB can wait to be sent; send or wait",
            MAX_PENDING_BYTES / (1024 * 1024)
        ),
    )
}

/// What an upload does when a row for its id already exists: the same upload again returns it, and
/// anything else is a `conflict` that says nothing about whose it is.
fn repeat_of(
    existing: &AttachmentRow,
    actor: UserId,
    conversation: ConversationId,
    sha256: Option<&[u8; 32]>,
) -> Result<Attachment, AppError> {
    let same = existing.uploader_id == actor.as_uuid()
        && existing.conversation_id == conversation.as_uuid()
        && existing.deleted_at.is_none()
        && sha256.is_none_or(|hash| existing.sha256 == hash[..]);
    if same {
        existing.to_attachment()
    } else {
        Err(AppError::conflict(
            "that attachment id is already used for something else",
        ))
    }
}

/// Stores an uploaded file for a conversation.
///
/// In order: the caller must be able to post in the conversation (`not_found`, `forbidden`);
/// `Content-Length` is required and must be within the limit, which is checked before a byte is
/// read (`payload_too_large`); an id that already exists is the same upload again only if it is the
/// caller's, for this conversation, and the same size (else `conflict`, without saying whose);
/// the pending limits (`limit_reached`); then the body is streamed to disk and hashed, the type is
/// decided from its first bytes, an image is decoded (`unsupported_media` if it cannot be, or has
/// too many pixels), and finally, under locks, the file and its thumbnail are stored and the row
/// inserted. The same id with different bytes is a `conflict`; with the same bytes it is the same
/// attachment, whether the first upload has finished or is racing this one.
pub async fn upload(
    state: &AppState,
    actor: UserId,
    conversation: ConversationId,
    id: AttachmentId,
    headers: &HeaderMap,
    body: Body,
) -> Result<Attachment, AppError> {
    let attachments = &state.attachments;
    let options = &attachments.options;
    let pool = &state.pool;

    let mut conn = pool.acquire().await?;
    let loaded = conversations::load(&mut conn, actor, conversation)
        .await?
        .filter(conversations::Loaded::can_read)
        .ok_or_else(|| AppError::not_found("conversation not found"))?;
    if !loaded.access().can_post() {
        return Err(AppError::forbidden("you cannot post in this conversation"));
    }
    let workspace = loaded.workspace_id;
    let declared = declared_length(headers, options.max_bytes)?;
    let name = name_from(headers)?;
    let declared_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();

    let existing = find(&mut conn, id.as_uuid()).await?;
    if let Some(existing) = &existing {
        let attachment = repeat_of(existing, actor, conversation, None)?;
        if attachment.size != declared {
            return Err(AppError::conflict(
                "that attachment id is already used for something else",
            ));
        }
    } else {
        let (files, bytes) = pending_of(&mut conn, actor.as_uuid()).await?;
        if files >= MAX_PENDING_FILES || bytes + declared as i64 > MAX_PENDING_BYTES {
            return Err(too_much_pending());
        }
    }
    drop(conn);

    // Stream the body to a staging file. Whatever happens next, it is discarded or committed.
    let stream = body_stream(body);
    let staged = attachments
        .store
        .stage(id.as_uuid(), stream, options.max_bytes)
        .await
        .map_err(|error| match error {
            StoreError::TooLarge => AppError::new(
                ErrorCode::PayloadTooLarge,
                format!("a file can be at most {} bytes", options.max_bytes),
            ),
            StoreError::Io(e) if e.kind() == std::io::ErrorKind::ConnectionAborted => {
                AppError::invalid("the upload was interrupted")
            }
            StoreError::Io(e) => AppError::internal(format!("cannot store the upload: {e}")),
        })?;
    let upload = Upload {
        actor,
        conversation,
        workspace,
        id,
        name,
        declared_type,
        declared,
    };
    let result = finish_upload(state, &upload, &staged).await;
    // Whatever happened, the staged file is no longer wanted: committed, it has been moved (and
    // this finds nothing); a repeat of an earlier upload, or a refusal, leaves it here.
    attachments.store.discard(&staged.token).await;
    result
}

/// What an upload knows before its body has been read.
struct Upload {
    actor: UserId,
    conversation: ConversationId,
    workspace: lazy_domain::WorkspaceId,
    id: AttachmentId,
    name: String,
    declared_type: String,
    declared: u64,
}

async fn finish_upload(
    state: &AppState,
    upload: &Upload,
    staged: &store::Staged,
) -> Result<Attachment, AppError> {
    let Upload {
        actor,
        conversation,
        workspace,
        id,
        name,
        declared_type,
        declared,
    } = upload;
    let (actor, conversation, id) = (*actor, *conversation, *id);
    let attachments = &state.attachments;
    let pool = &state.pool;
    if staged.size != *declared {
        return Err(AppError::invalid(
            "the body is not as long as Content-Length said",
        ));
    }

    // Is this a repeat of an upload that already finished?
    if let Some(existing) = find(&mut *pool.acquire().await?, id.as_uuid()).await? {
        return repeat_of(&existing, actor, conversation, Some(&staged.sha256));
    }

    let media_type = resolve_media_type(declared_type, &staged.prefix);
    let analysis = if is_previewable(&media_type) {
        let _slot = attachments
            .thumbnail_slots
            .acquire()
            .await
            .map_err(|_| AppError::internal("the image decoder is shut down"))?;
        let store = Arc::clone(&attachments.store);
        let token = staged.token.clone();
        let media = media_type.clone();
        let analyzed = tokio::task::spawn_blocking(move || {
            images::analyze(&media, || store.staged_source(&token))
        })
        .await
        .map_err(|e| AppError::internal(format!("image decoding failed: {e}")))?;
        match analyzed {
            Ok(analysis) => Some(analysis),
            Err(images::Refused(why)) => {
                return Err(AppError::new(ErrorCode::UnsupportedMedia, why));
            }
        }
    } else {
        None
    };

    // Commit under locks: one upload of an id at a time, and one person's pending limits at a time.
    let mut tx = pool.begin().await?;
    for lock in [
        format!("attachment-user:{actor}"),
        format!("attachment:{id}"),
    ] {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(lock)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(existing) = find(&mut tx, id.as_uuid()).await? {
        return repeat_of(&existing, actor, conversation, Some(&staged.sha256));
    }
    let (files, bytes) = pending_of(&mut tx, actor.as_uuid()).await?;
    if files >= MAX_PENDING_FILES || bytes + staged.size as i64 > MAX_PENDING_BYTES {
        return Err(too_much_pending());
    }

    let key = ObjectKey::original(id.as_uuid());
    let thumb_key = ObjectKey::thumbnail(id.as_uuid());
    attachments
        .store
        .commit(&staged.token, key)
        .await
        .map_err(|e| AppError::internal(format!("cannot store the file: {e}")))?;
    let stored = async {
        if let Some(analysis) = &analysis {
            attachments
                .store
                .put_small(thumb_key, analysis.thumbnail.clone())
                .await
                .map_err(|e| AppError::internal(format!("cannot store the thumbnail: {e}")))?;
        }
        if attachments.options.fail_between_file_and_row {
            return Err(AppError::internal(
                "simulated failure between the file and its row",
            ));
        }
        sqlx::query(
            "INSERT INTO attachments (id, workspace_id, conversation_id, uploader_id, name, media_type, \
                                      size, sha256, width, height, has_thumbnail, thumbnail_type) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(id.as_uuid())
        .bind(workspace.as_uuid())
        .bind(conversation.as_uuid())
        .bind(actor.as_uuid())
        .bind(name)
        .bind(&media_type)
        .bind(staged.size as i64)
        .bind(&staged.sha256[..])
        .bind(analysis.as_ref().map(|a| a.width as i32))
        .bind(analysis.as_ref().map(|a| a.height as i32))
        .bind(analysis.is_some())
        .bind(analysis.as_ref().map(|a| a.thumbnail_type))
        .execute(&mut *tx)
        .await?;
        Ok::<(), AppError>(())
    }
    .await;
    let committed = match stored {
        Ok(()) => tx.commit().await.map_err(AppError::from),
        Err(error) => Err(error),
    };
    if let Err(error) = committed {
        // The files were stored but the row was not: take them back out if we can, and leave the
        // sweep to find them if we cannot (a simulated crash leaves them on purpose).
        if !attachments.options.fail_between_file_and_row {
            let _ = attachments.store.delete(key).await;
            let _ = attachments.store.delete(thumb_key).await;
        }
        return Err(error);
    }

    Ok(Attachment {
        id,
        name: name.clone(),
        media_type,
        size: staged.size,
        sha256: hex(&staged.sha256),
        width: analysis.as_ref().map(|a| a.width),
        height: analysis.as_ref().map(|a| a.height),
        has_thumbnail: analysis.is_some(),
    })
}

// --- Attaching to a message --------------------------------------------------------------------

/// Checks, in the transaction that is about to commit a message, that the files it names are the
/// sender's, for this conversation, pending, and unattached; locks them (in id order, so two
/// messages naming the same files cannot deadlock) and returns them in the order named.
///
/// Nothing sent is the answer to any failure: more than [`MAX_ATTACHMENTS_PER_MESSAGE`] or a file
/// named twice is `invalid_request`; a file that does not exist, is someone else's, belongs to
/// another conversation, or has been removed is `not_found` (the same, so ownership is not
/// revealed); one already on a message is `invalid_request`.
pub(crate) async fn claim_for_send(
    conn: &mut PgConnection,
    sender: UserId,
    conversation: ConversationId,
    ids: &[AttachmentId],
) -> Result<Vec<Attachment>, AppError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    if ids.len() > MAX_ATTACHMENTS_PER_MESSAGE {
        return Err(AppError::invalid(format!(
            "a message can have at most {MAX_ATTACHMENTS_PER_MESSAGE} files"
        )));
    }
    let wanted: Vec<Uuid> = ids.iter().map(|id| id.as_uuid()).collect();
    let mut sorted = wanted.clone();
    sorted.sort();
    sorted.dedup();
    if sorted.len() != wanted.len() {
        return Err(AppError::invalid("a file is named twice"));
    }
    let rows: Vec<AttachmentRow> = sqlx::query_as(attachment_select!(
        "WHERE id = ANY($1) ORDER BY id FOR UPDATE"
    ))
    .bind(&wanted)
    .fetch_all(&mut *conn)
    .await?;
    let mut by_id: HashMap<Uuid, AttachmentRow> =
        rows.into_iter().map(|row| (row.id, row)).collect();
    let mut claimed = Vec::with_capacity(ids.len());
    for id in &wanted {
        let row = by_id
            .remove(id)
            .filter(|row| {
                row.uploader_id == sender.as_uuid()
                    && row.conversation_id == conversation.as_uuid()
                    && row.deleted_at.is_none()
            })
            .ok_or_else(not_found)?;
        if row.message_id.is_some() {
            return Err(AppError::invalid("a file is already on a message"));
        }
        claimed.push(row.to_attachment()?);
    }
    Ok(claimed)
}

/// Puts `attachments` on `message`, in order. They were locked by [`claim_for_send`].
pub(crate) async fn attach(
    conn: &mut PgConnection,
    message: Uuid,
    attachments: &[Attachment],
) -> Result<(), AppError> {
    if attachments.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = attachments.iter().map(|a| a.id.as_uuid()).collect();
    let ordinals: Vec<i16> = (0..attachments.len() as i16).collect();
    sqlx::query(
        "UPDATE attachments a SET message_id = $1, ordinal = o.ordinal \
         FROM unnest($2::uuid[], $3::smallint[]) AS o(id, ordinal) WHERE a.id = o.id",
    )
    .bind(message)
    .bind(&ids)
    .bind(&ordinals)
    .execute(conn)
    .await?;
    Ok(())
}

/// The attachments of each message in `messages` that has any, in the order they were sent, with
/// one query for all of them.
pub(crate) async fn for_messages(
    conn: &mut PgConnection,
    messages: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Attachment>>, AppError> {
    if messages.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<AttachmentRow> = sqlx::query_as(attachment_select!(
        "WHERE message_id = ANY($1) AND deleted_at IS NULL ORDER BY message_id, ordinal"
    ))
    .bind(messages)
    .fetch_all(conn)
    .await?;
    let mut grouped: HashMap<Uuid, Vec<Attachment>> = HashMap::new();
    for row in rows {
        if let Some(message) = row.message_id {
            grouped
                .entry(message)
                .or_default()
                .push(row.to_attachment()?);
        }
    }
    Ok(grouped)
}

/// A message was deleted: its files can no longer be read. The bytes follow in the cleanup job.
pub(crate) async fn mark_message_deleted(
    conn: &mut PgConnection,
    message: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE attachments SET deleted_at = now() WHERE message_id = $1 AND deleted_at IS NULL",
    )
    .bind(message)
    .execute(conn)
    .await?;
    Ok(())
}

// --- Downloading -------------------------------------------------------------------------------

/// Whether an `If-None-Match` header names `etag` (or `*`), weak validators included.
pub(crate) fn etag_matches(header: &HeaderValue, etag: &str) -> bool {
    let Ok(text) = header.to_str() else {
        return false;
    };
    text.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
    })
}

/// `filename*=UTF-8''...` per RFC 8187: everything but the unreserved characters percent-encoded.
fn encode_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The `Content-Disposition` of a download: shown inline only for the image types that are safe to
/// show, offered as a file otherwise, with the name both as an ASCII fallback and in full.
fn disposition(media_type: &str, name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let how = if is_previewable(media_type) {
        "inline"
    } else {
        "attachment"
    };
    format!(
        "{how}; filename=\"{fallback}\"; filename*=UTF-8''{}",
        encode_filename(name)
    )
}

/// Serves an attachment's bytes, or its thumbnail, to `actor`.
///
/// Who may: the uploader while it is pending; anyone who can read the conversation once it is on a
/// message. Anyone else, a file whose message was deleted, one that is gone, and one that never
/// existed all get the same `not_found`. The answer carries the stored media type, `inline` only
/// for the previewable image types, `nosniff`, a CSP that allows nothing, an `ETag` of the content
/// (a matching `If-None-Match` is a 304), and an immutable private cache lifetime; the body is
/// streamed from the store, never held whole. `HEAD` is the same without the body.
pub async fn download(
    state: &AppState,
    actor: UserId,
    id: AttachmentId,
    thumbnail: bool,
    headers: &HeaderMap,
) -> Result<Response, AppError> {
    let pool = &state.pool;
    let mut conn = pool.acquire().await?;
    let row = find(&mut conn, id.as_uuid()).await?.ok_or_else(not_found)?;
    if row.deleted_at.is_some() {
        return Err(not_found());
    }
    match row.message_id {
        None => {
            if row.uploader_id != actor.as_uuid() {
                return Err(not_found());
            }
            // Their own pending upload, but only while they are still in the workspace.
            conversations::load(
                &mut conn,
                actor,
                ConversationId::from_uuid(row.conversation_id),
            )
            .await?
            .filter(|loaded| loaded.facts.workspace_role.is_some())
            .ok_or_else(not_found)?;
        }
        Some(_) => {
            conversations::load(
                &mut conn,
                actor,
                ConversationId::from_uuid(row.conversation_id),
            )
            .await?
            .filter(conversations::Loaded::can_read)
            .ok_or_else(not_found)?;
        }
    }
    drop(conn);
    if thumbnail && !row.has_thumbnail {
        return Err(not_found());
    }

    let sha = hex(&row.sha256);
    let (etag, media_type, key) = if thumbnail {
        (
            format!("\"{sha}-thumb\""),
            row.thumbnail_type
                .clone()
                .unwrap_or_else(|| "image/jpeg".to_owned()),
            ObjectKey::thumbnail(id.as_uuid()),
        )
    } else {
        (
            format!("\"{sha}\""),
            row.media_type.clone(),
            ObjectKey::original(id.as_uuid()),
        )
    };
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
        .is_some_and(|value| etag_matches(value, &etag))
    {
        return Ok((StatusCode::NOT_MODIFIED, response_headers).into_response());
    }

    let object = state
        .attachments
        .store
        .open(key)
        .await
        .map_err(|e| AppError::internal(format!("cannot read the file: {e}")))?;
    let Some(object) = object else {
        // A row with no file: not supposed to happen (the file is stored first). Say so loudly.
        tracing::error!("an attachment's file is missing from the store");
        return Err(not_found());
    };
    let mut put = |name, value: &str| {
        if let Ok(value) = HeaderValue::from_str(value) {
            response_headers.insert(name, value);
        }
    };
    put(CONTENT_TYPE, &media_type);
    put(CONTENT_LENGTH, &object.size.to_string());
    let shown = if thumbnail {
        "inline; filename=\"thumbnail\"".to_owned()
    } else {
        disposition(&media_type, &row.name)
    };
    put(CONTENT_DISPOSITION, &shown);
    let body = Body::from_stream(ReaderStream::with_capacity(object.reader, 64 * 1024));
    Ok((StatusCode::OK, response_headers, body).into_response())
}

// --- Cleanup ---------------------------------------------------------------------------------

/// What a cleanup pass did, for the log (counts, never names).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CleanupReport {
    /// Pending uploads past their time, marked deleted.
    pub expired: usize,
    /// Rows removed after their files were.
    pub removed: usize,
    /// Files with no row, removed by the sweep.
    pub orphans: usize,
    /// Staged uploads that were never finished, removed by the sweep.
    pub stale_uploads: usize,
    /// Images of deleted custom emoji, removed.
    pub emoji_images: usize,
}

const BATCH: i64 = 500;

/// Removes what is no longer wanted: pending uploads older than `ttl` are marked deleted (so they
/// can no longer be read or attached), then the files of every row marked deleted (by this pass, or
/// by a message's deletion) are removed, and then the rows.
///
/// The files go before the rows, so a crash in between leaves rows marked deleted for the next
/// pass to finish (removing a file that is already gone is not an error). Rows are claimed with
/// `SKIP LOCKED`, so the pass never waits on, or deadlocks with, a send that is attaching them.
pub async fn cleanup(
    pool: &PgPool,
    store: &dyn AttachmentStore,
    ttl: Duration,
) -> Result<CleanupReport, AppError> {
    let mut report = CleanupReport::default();
    loop {
        let marked = sqlx::query(
            "UPDATE attachments SET deleted_at = now() WHERE id IN ( \
                 SELECT id FROM attachments \
                 WHERE message_id IS NULL AND deleted_at IS NULL \
                   AND created_at < now() - make_interval(secs => $1) \
                 ORDER BY id LIMIT $2 FOR UPDATE SKIP LOCKED)",
        )
        .bind(ttl.as_secs_f64())
        .bind(BATCH)
        .execute(pool)
        .await?
        .rows_affected() as usize;
        report.expired += marked;
        if marked < BATCH as usize {
            break;
        }
    }
    loop {
        let doomed: Vec<(Uuid, bool)> = sqlx::query_as(
            "SELECT id, has_thumbnail FROM attachments WHERE deleted_at IS NOT NULL \
             ORDER BY deleted_at, id LIMIT $1",
        )
        .bind(BATCH)
        .fetch_all(pool)
        .await?;
        if doomed.is_empty() {
            break;
        }
        let mut removed: Vec<Uuid> = Vec::with_capacity(doomed.len());
        for (id, has_thumbnail) in &doomed {
            let original = store.delete(ObjectKey::original(*id)).await;
            let thumb = if *has_thumbnail {
                store.delete(ObjectKey::thumbnail(*id)).await
            } else {
                Ok(false)
            };
            match (original, thumb) {
                (Ok(_), Ok(_)) => removed.push(*id),
                _ => tracing::warn!("an attachment's file could not be removed; will retry"),
            }
        }
        if removed.is_empty() {
            break;
        }
        report.removed += sqlx::query("DELETE FROM attachments WHERE id = ANY($1)")
            .bind(&removed)
            .execute(pool)
            .await?
            .rows_affected() as usize;
        if removed.len() < doomed.len() {
            break;
        }
    }
    report.emoji_images = crate::emoji::remove_deleted_images(pool, store).await?;
    Ok(report)
}

/// Removes files that have no row, and staged uploads that were never finished, once they are older
/// than `grace` (an upload in progress has a file or a staged file and no row for a moment, and
/// must not lose it).
pub async fn sweep_orphans(
    pool: &PgPool,
    store: &dyn AttachmentStore,
    grace: Duration,
) -> Result<CleanupReport, AppError> {
    let mut report = CleanupReport::default();
    let now = SystemTime::now();
    let old_enough =
        |modified: SystemTime| now.duration_since(modified).is_ok_and(|age| age >= grace);
    let entries = store
        .list()
        .await
        .map_err(|e| AppError::internal(format!("cannot list the attachment store: {e}")))?;
    let mut candidates: Vec<ObjectKey> = Vec::new();
    for entry in entries {
        if !old_enough(entry.modified) {
            continue;
        }
        match entry.what {
            EntryKind::Staged(token) => {
                store.discard(&token).await;
                report.stale_uploads += 1;
            }
            EntryKind::Object(key) => candidates.push(key),
        }
    }
    for chunk in candidates.chunks(1000) {
        let ids: Vec<Uuid> = chunk.iter().map(|key| key.id).collect();
        // A file is accounted for by a row of its own kind: an attachment's original or thumbnail
        // by the attachment, a custom emoji's image by an emoji whose image is still wanted. (An
        // emoji and an attachment may have the same id; neither accounts for the other's file.)
        let attached: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM attachments WHERE id = ANY($1)")
                .bind(&ids)
                .fetch_all(pool)
                .await?;
        let emoji: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM custom_emoji WHERE id = ANY($1) AND NOT image_removed",
        )
        .bind(&ids)
        .fetch_all(pool)
        .await?;
        let accounted = |key: &ObjectKey| match key.kind {
            ObjectKind::Emoji => emoji.contains(&key.id),
            ObjectKind::Original | ObjectKind::Thumbnail => attached.contains(&key.id),
        };
        for key in chunk.iter().filter(|key| !accounted(key)) {
            match store.delete(*key).await {
                Ok(_) => report.orphans += 1,
                Err(_) => tracing::warn!("an orphaned attachment file could not be removed"),
            }
        }
    }
    Ok(report)
}

/// The running cleanup jobs. Cancel the token passed to [`spawn_jobs`], then [`Jobs::join`].
pub struct Jobs {
    tasks: tokio::task::JoinSet<()>,
}

impl Jobs {
    pub async fn join(mut self) {
        while let Some(result) = self.tasks.join_next().await {
            if let Err(error) = result {
                tracing::error!(error = %error.describe(), "attachment job failed");
            }
        }
    }
}

/// Starts the hourly cleanup and the daily sweep (their intervals are in the options), each
/// running once at start-up.
pub fn spawn_jobs(
    pool: PgPool,
    attachments: Arc<Attachments>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Jobs {
    let mut tasks = tokio::task::JoinSet::new();
    {
        let (pool, attachments, shutdown) =
            (pool.clone(), Arc::clone(&attachments), shutdown.clone());
        tasks.spawn(async move {
            loop {
                match cleanup(
                    &pool,
                    attachments.store.as_ref(),
                    attachments.options.pending_ttl,
                )
                .await
                {
                    Ok(report) if report != CleanupReport::default() => {
                        tracing::info!(
                            expired = report.expired,
                            removed = report.removed,
                            "cleaned up attachments"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(error = %error.describe(), "attachment cleanup failed; will retry"),
                }
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(attachments.options.cleanup_every) => {}
                }
            }
        });
    }
    tasks.spawn(async move {
        loop {
            match sweep_orphans(
                &pool,
                attachments.store.as_ref(),
                attachments.options.sweep_grace,
            )
            .await
            {
                Ok(report) if report != CleanupReport::default() => {
                    tracing::info!(
                        orphans = report.orphans,
                        stale_uploads = report.stale_uploads,
                        "swept attachment files that no row accounts for"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error = %error.describe(), "attachment sweep failed; will retry")
                }
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(attachments.options.sweep_every) => {}
            }
        }
    });
    Jobs { tasks }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_percent_decoded_and_bad_escapes_are_refused() {
        assert_eq!(
            percent_decode("report%20final.pdf").as_deref(),
            Some("report final.pdf")
        );
        assert_eq!(percent_decode("caf%C3%A9").as_deref(), Some("café"));
        assert_eq!(percent_decode("a+b").as_deref(), Some("a+b"));
        assert_eq!(percent_decode("plain").as_deref(), Some("plain"));
        for bad in ["%", "%4", "%zz", "%FF", "%C3", "100%"] {
            assert_eq!(percent_decode(bad), None, "{bad}");
        }
    }

    #[test]
    fn filenames_are_encoded_for_a_header() {
        assert_eq!(encode_filename("report 1.pdf"), "report%201.pdf");
        assert_eq!(encode_filename("café.txt"), "caf%C3%A9.txt");
        assert_eq!(encode_filename("a;b'c\"d"), "a%3Bb%27c%22d");
    }

    #[test]
    fn only_previewable_images_are_shown_inline() {
        assert!(disposition("image/png", "a.png").starts_with("inline;"));
        assert!(disposition("application/pdf", "a.pdf").starts_with("attachment;"));
        assert!(disposition("application/octet-stream", "a.html").starts_with("attachment;"));
        let shown = disposition("application/zip", "naïve résumé.zip");
        assert!(shown.contains("filename=\"na_ve r_sum_.zip\""));
        assert!(shown.contains("filename*=UTF-8''na%C3%AFve%20r%C3%A9sum%C3%A9.zip"));
    }

    #[test]
    fn if_none_match_understands_lists_weak_validators_and_stars() {
        let header = |text: &str| HeaderValue::from_str(text).unwrap();
        assert!(etag_matches(&header("\"abc\""), "\"abc\""));
        assert!(etag_matches(&header("W/\"abc\""), "\"abc\""));
        assert!(etag_matches(&header("\"x\", \"abc\""), "\"abc\""));
        assert!(etag_matches(&header("*"), "\"abc\""));
        assert!(!etag_matches(&header("\"abd\""), "\"abc\""));
    }
}
