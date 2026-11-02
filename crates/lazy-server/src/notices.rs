//! Notices (D44): the messages the server posts when something happens in a conversation.
//!
//! A notice is a message whose author is the person who did it, with a [`Notice`] in `notice` and
//! the same in words as its document ([`Notice::describe`], with the names as they are now). It
//! is posted in the transaction that made the change, after that change's own events, so it
//! takes the next position and goes to the conversation's members like any message. Only a
//! drop-in's notice counts as unread (and notifies, by the person's usual rules); the others are
//! `quiet`. Notices are not found by search (their `plain_text` is empty). Nobody edits a notice; the server updates a drop-in's
//! notice as it goes ([`update`]).

use lazy_domain::{
    ConversationId, ConversationKind, EventSeq, MessageDocument, MessageId, Notice, UserId,
    WorkspaceId,
};
use lazy_protocol::{Event, Mentions, Message};
use sqlx::PgConnection;
use sqlx::types::Json;
use uuid::Uuid;

use crate::db;
use crate::error::AppError;
use crate::events::{self, Audience, NewEvent};

/// The notice in words, as its document: names and the conversation as they are now.
async fn words(
    conn: &mut PgConnection,
    conversation: ConversationId,
    notice: &Notice,
) -> Result<MessageDocument, AppError> {
    let (kind, name): (String, Option<String>) =
        sqlx::query_as("SELECT kind, name FROM conversations WHERE id = $1")
            .bind(conversation.as_uuid())
            .fetch_one(&mut *conn)
            .await?;
    let place = match (db::kind_from_db(&kind)?, name) {
        (ConversationKind::Channel, Some(name)) => format!("#{name}"),
        _ => "the conversation".to_owned(),
    };
    let people: Vec<Uuid> = match notice {
        Notice::Added { users } => users.iter().map(|user| user.as_uuid()).collect(),
        Notice::Removed { user } => vec![user.as_uuid()],
        _ => Vec::new(),
    };
    let names: Vec<(Uuid, String)> = if people.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as("SELECT id, display_name FROM users WHERE id = ANY($1)")
            .bind(&people)
            .fetch_all(&mut *conn)
            .await?
    };
    let name = |user: UserId| {
        names
            .iter()
            .find(|(id, _)| *id == user.as_uuid())
            .map_or_else(|| "someone".to_owned(), |(_, name)| name.clone())
    };
    let text = notice.describe(&place, &name);
    MessageDocument::from_plain_text(&text)
        .or_else(|_| MessageDocument::from_plain_text("posted a notice"))
        .map_err(|e| AppError::internal(format!("a notice's words: {e}")))
}

/// Posts `notice` in `conversation` as `author`, at the next position, and announces it to the
/// members. Call it after the change's own writes and events: it takes the workspace's lock.
pub(crate) async fn post(
    conn: &mut PgConnection,
    workspace_id: WorkspaceId,
    conversation: ConversationId,
    author: UserId,
    notice: Notice,
) -> Result<Message, AppError> {
    let document = words(conn, conversation, &notice).await?;
    let id = MessageId::generate();
    let (seq, now) = events::next_position(conn, workspace_id).await?;
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, author_id, document, plain_text, revision, seq, created_at, \
                               mentions, mentions_channel, thread_root_id, in_channel, listed, notice, quiet) \
         VALUES ($1, $2, $3, $4, '', 1, $5, $6, $7, false, NULL, false, true, $8, $9)",
    )
    .bind(id.as_uuid())
    .bind(conversation.as_uuid())
    .bind(author.as_uuid())
    .bind(Json(&document))
    .bind(seq.get())
    .bind(now.as_offset_date_time())
    .bind(Json(Mentions::default()))
    .bind(Json(&notice))
    .bind(!notice.counts_as_unread())
    .execute(&mut *conn)
    .await?;
    // Their own notice is read by whoever did it, if they are (still) in the conversation.
    sqlx::query(
        "UPDATE conversation_user_state SET last_read = GREATEST(last_read, $3) \
         WHERE user_id = $1 AND conversation_id = $2",
    )
    .bind(author.as_uuid())
    .bind(conversation.as_uuid())
    .bind(seq.get())
    .execute(&mut *conn)
    .await?;
    let message = Message {
        id,
        conversation_id: conversation,
        author_id: author,
        document,
        rich: None,
        notice: Some(notice),
        created_at: now,
        revision: 1,
        seq,
        edited_at: None,
        thread_root: None,
        in_channel: false,
        thread: None,
        attachments: Vec::new(),
        mentions: Mentions::default(),
        reactions: Vec::new(),
        reactions_as_of: EventSeq::ZERO,
        deleted: false,
    };
    events::insert_event(
        conn,
        NewEvent {
            workspace_id,
            seq,
            occurred_at: now,
            kind: "message_created",
            conversation_id: Some(conversation),
            operation_id: None,
            audience: Audience::Members,
            event: &Event::MessageCreated {
                message: message.clone(),
                operation_id: None,
            },
        },
    )
    .await?;
    Ok(message)
}

/// Replaces a notice's content (a drop-in's, as people come and go) and announces it as an edit
/// of the message, without the "edited" mark. A notice that was deleted, or is gone, is left
/// alone. Takes the workspace's lock: call it last.
pub(crate) async fn update(
    conn: &mut PgConnection,
    workspace_id: WorkspaceId,
    conversation: ConversationId,
    id: MessageId,
    notice: Notice,
) -> Result<(), AppError> {
    let document = words(conn, conversation, &notice).await?;
    let updated = sqlx::query(
        "UPDATE messages SET notice = $2, document = $3, revision = revision + 1 \
         WHERE id = $1 AND notice IS NOT NULL AND NOT deleted",
    )
    .bind(id.as_uuid())
    .bind(Json(&notice))
    .bind(Json(&document))
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if updated == 0 {
        return Ok(());
    }
    let Some(message) = crate::messages::fetch_message(conn, id).await? else {
        return Ok(());
    };
    events::append_all(
        conn,
        workspace_id,
        vec![events::PendingEvent {
            kind: "message_edited",
            conversation_id: Some(conversation),
            operation_id: None,
            audience: Audience::Members,
            event: Event::MessageEdited {
                message,
                operation_id: None,
                previous_mentions: Mentions::default(),
            },
        }],
    )
    .await?;
    Ok(())
}
