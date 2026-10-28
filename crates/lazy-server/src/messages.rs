//! Sending messages, reading history, and looking up operation outcomes.

use lazy_domain::{
    ConversationId, EventSeq, MessageDocument, MessageId, OperationId, Timestamp, UserId,
};
use lazy_protocol::{
    Event, HistoryPage, HistoryQuery, Message, OperationStatus, SendMessageRequest,
};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::conversations::{self, Lock};
use crate::error::AppError;
use crate::events::{self, Audience, NewEvent};
use crate::receipts::{self, ReceiptKind, ReceiptTarget};

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

/// Commits a message and returns the acknowledgment.
///
/// One transaction authorizes the caller, claims the operation receipt, reserves the next event
/// position, and inserts the message and its event. The result is returned only after commit.
/// Repeating a committed operation with the same content returns the original message; the same
/// operation id with different content is a conflict.
///
/// The transaction holds the conversation row `FOR SHARE`: any number of sends run together, but
/// never alongside an archive or a membership change, so no message commits after an archive or
/// after its author's removal in the log. It also moves the author's read position to the new
/// message, without an event: a person who has just written has read everything before it.
pub async fn send_message(
    pool: &PgPool,
    user: UserId,
    conversation: ConversationId,
    request: SendMessageRequest,
) -> Result<Message, AppError> {
    let fingerprint = crate::crypto::request_fingerprint(conversation, &request.document)?;

    let (mut tx, loaded) =
        conversations::begin_visible(pool, user, conversation, Lock::Shared).await?;
    if !loaded.access().can_post() {
        return Err(AppError::forbidden("you cannot post in this conversation"));
    }

    // Claim the operation. A concurrent transaction holding the same (user, operation) makes this
    // wait for its outcome, so exactly one of two simultaneous duplicates inserts the message.
    let message_id = MessageId::generate();
    if let Some(existing) = receipts::claim(
        &mut tx,
        user,
        request.operation_id,
        ReceiptKind::SendMessage,
        &fingerprint,
        ReceiptTarget::Message(message_id),
    )
    .await?
    {
        existing.ensure_same(ReceiptKind::SendMessage, &fingerprint)?;
        let original = existing
            .message_id
            .map(MessageId::from_uuid)
            .ok_or_else(|| AppError::internal("receipt has no message"))?;
        return fetch_message(&mut tx, original)
            .await?
            .ok_or_else(|| AppError::internal("receipted message vanished"));
    }

    let (seq, now) = events::next_position(&mut tx, loaded.workspace_id).await?;
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
    sqlx::query(
        "INSERT INTO conversation_user_state (user_id, conversation_id, last_read) VALUES ($1, $2, $3) \
         ON CONFLICT (user_id, conversation_id) DO UPDATE \
         SET last_read = GREATEST(conversation_user_state.last_read, excluded.last_read)",
    )
    .bind(user.as_uuid())
    .bind(conversation.as_uuid())
    .bind(seq.get())
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
        NewEvent {
            workspace_id: loaded.workspace_id,
            seq,
            occurred_at: now,
            kind: "message_created",
            conversation_id: Some(conversation),
            operation_id: Some(request.operation_id),
            audience: Audience::Members,
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

/// Loads a message by id.
async fn fetch_message(
    conn: &mut PgConnection,
    id: MessageId,
) -> Result<Option<Message>, AppError> {
    let row: Option<MessageRow> =
        sqlx::query_as(message_select!("FROM messages m WHERE m.id = $1"))
            .bind(id.as_uuid())
            .fetch_optional(conn)
            .await?;
    row.map(MessageRow::into_message).transpose()
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
    conversations::load(&mut conn, user, conversation)
        .await?
        .filter(conversations::Loaded::can_read)
        .ok_or_else(|| AppError::not_found("conversation not found"))?;

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
    let Some(receipt) = receipts::find(&mut conn, user, operation).await? else {
        return Ok(OperationStatus::NotFound);
    };
    if let Some(id) = receipt.message_id {
        let message = fetch_message(&mut conn, MessageId::from_uuid(id))
            .await?
            .ok_or_else(|| AppError::internal("receipted message vanished"))?;
        return Ok(OperationStatus::Committed { message });
    }
    match conversations::operation_conversation(&mut conn, user, operation).await? {
        Some(conversation) => Ok(OperationStatus::ConversationCommitted { conversation }),
        None => Ok(OperationStatus::NotFound),
    }
}
