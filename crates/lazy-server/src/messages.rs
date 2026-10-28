//! Sending messages, reading history, and looking up operation outcomes.

use lazy_domain::{
    ChannelAccess, ConversationId, EventSeq, MessageDocument, MessageId, OperationId, Timestamp,
    UserId, WorkspaceId,
};
use lazy_protocol::{
    Event, HistoryPage, HistoryQuery, Message, OperationStatus, SendMessageRequest,
};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::db;
use crate::error::AppError;
use crate::events;

/// History page size when the caller does not ask for one, and the largest it may ask for.
pub const DEFAULT_HISTORY_LIMIT: u32 = 50;
pub const MAX_HISTORY_LIMIT: u32 = 100;

/// A message row, selected with [`message_select!`].
#[derive(sqlx::FromRow)]
struct MessageRow {
    id: Uuid,
    conversation_id: Uuid,
    author_id: Uuid,
    document: Json<MessageDocument>,
    created_at: OffsetDateTime,
    revision: i32,
    seq: i64,
}

macro_rules! message_select {
    ($tail:literal) => {
        concat!(
            "SELECT m.id, m.conversation_id, m.author_id, m.document, m.created_at, m.revision, m.seq ",
            $tail
        )
    };
}

impl MessageRow {
    fn into_message(self) -> Result<Message, AppError> {
        Ok(Message {
            id: MessageId::from_uuid(self.id),
            conversation_id: ConversationId::from_uuid(self.conversation_id),
            author_id: UserId::from_uuid(self.author_id),
            document: self.document.0,
            created_at: Timestamp::from(self.created_at),
            revision: u32::try_from(self.revision)
                .map_err(|_| AppError::internal("stored message revision is negative"))?,
            seq: EventSeq::new(self.seq)
                .ok_or_else(|| AppError::internal("stored message position is negative"))?,
        })
    }
}

/// A conversation's workspace and a user's relationship to it.
#[derive(Debug, Clone, Copy)]
pub struct ChannelFacts {
    pub workspace_id: WorkspaceId,
    pub access: ChannelAccess,
}

/// Loads the facts needed to authorize `user` against `conversation`, or `None` if the
/// conversation does not exist.
pub async fn load_access(
    conn: &mut PgConnection,
    user: UserId,
    conversation: ConversationId,
) -> Result<Option<ChannelFacts>, AppError> {
    let row: Option<(Uuid, String, bool, Option<String>, bool)> = sqlx::query_as(
        "SELECT c.workspace_id, c.visibility, c.archived, wm.role, (cm.user_id IS NOT NULL) \
         FROM conversations c \
         LEFT JOIN workspace_members wm ON wm.workspace_id = c.workspace_id AND wm.user_id = $1 \
         LEFT JOIN conversation_members cm ON cm.conversation_id = c.id AND cm.user_id = $1 \
         WHERE c.id = $2",
    )
    .bind(user.as_uuid())
    .bind(conversation.as_uuid())
    .fetch_optional(conn)
    .await?;
    row.map(|(workspace_id, visibility, archived, role, is_member)| {
        Ok(ChannelFacts {
            workspace_id: WorkspaceId::from_uuid(workspace_id),
            access: ChannelAccess {
                workspace_role: role.as_deref().map(db::role_from_db).transpose()?,
                is_channel_member: is_member,
                visibility: db::visibility_from_db(&visibility)?,
                archived,
            },
        })
    })
    .transpose()
}

/// A conversation the caller may read; otherwise `NotFound`, so private channels do not reveal
/// their existence.
async fn readable_facts(
    conn: &mut PgConnection,
    user: UserId,
    conversation: ConversationId,
) -> Result<ChannelFacts, AppError> {
    load_access(conn, user, conversation)
        .await?
        .filter(|facts| facts.access.can_read())
        .ok_or_else(|| AppError::not_found("conversation not found"))
}

/// Commits a message and returns the acknowledgment.
///
/// One transaction authorizes the caller, claims the operation receipt, reserves the next event
/// position, and inserts the message and its event. The result is returned only after commit.
/// Repeating a committed operation with the same content returns the original message; the same
/// operation id with different content is a conflict.
pub async fn send_message(
    pool: &PgPool,
    user: UserId,
    conversation: ConversationId,
    request: SendMessageRequest,
) -> Result<Message, AppError> {
    let fingerprint = crate::crypto::request_fingerprint(conversation, &request.document)?;

    let mut tx = pool.begin().await?;
    let facts = readable_facts(&mut tx, user, conversation).await?;
    if !facts.access.can_post() {
        return Err(AppError::forbidden("you cannot post in this conversation"));
    }

    // Claim the operation. A concurrent transaction holding the same (user, operation) makes this
    // wait for its outcome, so exactly one of two simultaneous duplicates inserts the message.
    let message_id = MessageId::generate();
    let claimed = sqlx::query(
        "INSERT INTO operation_receipts (user_id, operation_id, request_hash, message_id) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (user_id, operation_id) DO NOTHING",
    )
    .bind(user.as_uuid())
    .bind(request.operation_id.as_uuid())
    .bind(&fingerprint[..])
    .bind(message_id.as_uuid())
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;

    if !claimed {
        let existing = fetch_receipt(&mut tx, user, request.operation_id).await?;
        tx.rollback().await?;
        return match existing {
            Some((stored_fingerprint, message)) if stored_fingerprint == fingerprint[..] => {
                Ok(message)
            }
            Some(_) => Err(AppError::conflict(
                "this operation id was already used for a different message",
            )),
            None => Err(AppError::internal("operation receipt vanished")),
        };
    }

    let (seq, now) = events::next_position(&mut tx, facts.workspace_id).await?;
    let plain_text = request.document.plain_text();
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, author_id, document, plain_text, revision, seq, created_at) \
         VALUES ($1, $2, $3, $4, $5, 1, $6, $7)",
    )
    .bind(message_id.as_uuid())
    .bind(conversation.as_uuid())
    .bind(user.as_uuid())
    .bind(Json(&request.document))
    .bind(&plain_text)
    .bind(seq.get())
    .bind(now.as_offset_date_time())
    .execute(&mut *tx)
    .await?;

    let message = Message {
        id: message_id,
        conversation_id: conversation,
        author_id: user,
        document: request.document,
        created_at: now,
        revision: 1,
        seq,
    };
    events::insert_event(
        &mut tx,
        events::NewEvent {
            workspace_id: facts.workspace_id,
            seq,
            occurred_at: now,
            kind: "message_created",
            conversation_id: Some(conversation),
            operation_id: Some(request.operation_id),
            event: &Event::MessageCreated {
                message: message.clone(),
                operation_id: Some(request.operation_id),
            },
        },
    )
    .await?;
    tx.commit().await?;
    Ok(message)
}

/// Looks up the receipt for an operation together with the message it produced.
async fn fetch_receipt(
    conn: &mut PgConnection,
    user: UserId,
    operation: OperationId,
) -> Result<Option<(Vec<u8>, Message)>, AppError> {
    #[derive(sqlx::FromRow)]
    struct ReceiptRow {
        request_hash: Vec<u8>,
        #[sqlx(flatten)]
        message: MessageRow,
    }
    let row: Option<ReceiptRow> = sqlx::query_as(concat!(
        "SELECT r.request_hash, m.id, m.conversation_id, m.author_id, m.document, m.created_at, m.revision, m.seq ",
        "FROM operation_receipts r JOIN messages m ON m.id = r.message_id ",
        "WHERE r.user_id = $1 AND r.operation_id = $2"
    ))
    .bind(user.as_uuid())
    .bind(operation.as_uuid())
    .fetch_optional(conn)
    .await?;
    row.map(|row| Ok((row.request_hash, row.message.into_message()?)))
        .transpose()
}

/// Pages a conversation's history, oldest first within the page.
pub async fn history(
    pool: &PgPool,
    user: UserId,
    conversation: ConversationId,
    query: HistoryQuery,
) -> Result<HistoryPage, AppError> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_HISTORY_LIMIT)
        .clamp(1, MAX_HISTORY_LIMIT);
    let mut conn = pool.acquire().await?;
    readable_facts(&mut conn, user, conversation).await?;

    let mut rows: Vec<MessageRow> = sqlx::query_as(message_select!(
        "FROM messages m WHERE m.conversation_id = $1 AND ($2::bigint IS NULL OR m.seq < $2) \
         ORDER BY m.seq DESC LIMIT $3"
    ))
    .bind(conversation.as_uuid())
    .bind(query.before.map(EventSeq::get))
    .bind(i64::from(limit) + 1)
    .fetch_all(&mut *conn)
    .await?;

    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    rows.reverse();
    let messages = rows
        .into_iter()
        .map(MessageRow::into_message)
        .collect::<Result<_, _>>()?;
    Ok(HistoryPage { messages, has_more })
}

/// The outcome of a previously submitted operation, as seen by the user who submitted it.
pub async fn operation_status(
    pool: &PgPool,
    user: UserId,
    operation: OperationId,
) -> Result<OperationStatus, AppError> {
    let mut conn = pool.acquire().await?;
    Ok(match fetch_receipt(&mut conn, user, operation).await? {
        Some((_, message)) => OperationStatus::Committed { message },
        None => OperationStatus::NotFound,
    })
}
