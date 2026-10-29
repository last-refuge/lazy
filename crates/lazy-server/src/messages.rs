//! Sending, editing, and deleting messages, reading history, and looking up operation outcomes.
//!
//! # Editing and deleting
//!
//! An edit replaces a message's document and bumps its revision; the message keeps the position it
//! was created at. A delete turns it into a tombstone (the fixed placeholder, `deleted` set, a
//! higher revision) and removes everything that held the old content: the prior revisions, and the
//! text carried by the events that announced it. Both are done in one transaction that locks, in
//! this order, **the conversation row, the message row, and then the workspace row for the event
//! position**, so they extend the order every other command uses and cannot deadlock with them or
//! with each other. The position is last because an edit that changes nothing, and a second delete,
//! must commit without appending an event, and a position cannot be reserved without one.
//!
//! # Threads
//!
//! A reply is a message with `thread_root_id` set, in the same conversation as its root, which is
//! always a top-level message. Sending one, and deleting one, also change the root's summary and
//! the follow state of the people involved, in the same transaction ([`crate::threads`]), so the
//! lock order extends to **the conversation row, the root's message row, the message row, and then
//! the workspace row for the event position**. The conversation's history lists top-level messages
//! and replies sent also to the channel (`messages.listed`), plus deleted roots that still have
//! replies, which stay as tombstones carrying their summary until the last reply goes.
//!
//! # Mentions
//!
//! A message that mentions people, groups, or `@channel` is checked and resolved when it is sent or
//! edited ([`compute_mentions`]): the result is stored with the message (`messages.mentions`, plus
//! one `message_mentions` row per person so unread mention counts are an index lookup) and returned
//! on it everywhere. An edit recomputes it and announces the set it replaced; a deletion announces
//! the set it cleared.

use std::collections::HashMap;

use lazy_domain::attachment::MAX_ATTACHMENTS_PER_MESSAGE;
use lazy_domain::mention::{MentionTarget, can_mention_channel};
use lazy_domain::message::{MessageFacts, deleted_placeholder};
use lazy_domain::{
    ConversationId, EventSeq, MAX_REACTION_KINDS, MessageDocument, MessageId, OperationId,
    ReactionEmoji, Timestamp, UserId,
};
use lazy_protocol::{
    EditMessageRequest, ErrorCode, Event, HistoryPage, HistoryQuery, Mentions, Message,
    OperationStatus, ReactionSummary, SendMessageRequest, SetReactionRequest,
};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::attachments;
use crate::conversations::{self, Lock};
use crate::emoji;
use crate::error::AppError;
use crate::events::{self, Audience, NewEvent};
use crate::groups;
use crate::receipts::{self, ReceiptKind, ReceiptTarget};
use crate::threads;

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
    edited_at: Option<OffsetDateTime>,
    deleted: bool,
    reactions_as_of: i64,
    mentions: Json<Mentions>,
    thread_root_id: Option<Uuid>,
    in_channel: bool,
}

macro_rules! message_select {
    ($tail:literal) => {
        concat!(
            "SELECT m.id, m.conversation_id, m.author_id, m.document, m.created_at, m.revision, m.seq, \
                    m.edited_at, m.deleted, m.reactions_as_of, m.mentions, m.thread_root_id, m.in_channel ",
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
            edited_at: self.edited_at.map(Timestamp::from),
            // The reactions themselves are attached by `attach_reactions`, for a whole page at
            // once; only their version is on the message row.
            reactions: Vec::new(),
            reactions_as_of: EventSeq::new(self.reactions_as_of)
                .ok_or_else(|| AppError::internal("stored reaction position is negative"))?,
            mentions: self.mentions.0,
            thread_root: self.thread_root_id.map(MessageId::from_uuid),
            in_channel: self.in_channel,
            // Attached with the reactions, for a whole page at once.
            thread: None,
            attachments: Vec::new(),
            deleted: self.deleted,
        })
    }

    /// Whether the conversation's history lists this message while it exists: a top-level message,
    /// or a reply that was also sent to the channel.
    fn is_listed(&self) -> bool {
        self.thread_root_id.is_none() || self.in_channel
    }
}

/// Decides who a message notifies, from the mentions in its document, and rejects a document that
/// cannot be sent as written.
///
/// A person named must be in the workspace and a group named must exist in it (`invalid_request`).
/// `@channel` needs [`can_mention_channel`] (`forbidden`; in the default channel it is for admins
/// and owners). The result is the people named plus the members of each group named, **limited to
/// current members of the conversation and never the author**, listed once each in a fixed order
/// (by id), with `channel` set if `@channel` was used. Naming someone who is not in the
/// conversation is allowed and notifies no one. Reads only; the caller holds the conversation row.
async fn compute_mentions(
    conn: &mut PgConnection,
    loaded: &conversations::Loaded,
    author: UserId,
    document: &MessageDocument,
) -> Result<Mentions, AppError> {
    let targets = document.mentions();
    if targets.is_empty() {
        return Ok(Mentions::default());
    }
    let (mut named, mut groups, mut channel) = (Vec::new(), Vec::new(), false);
    for target in targets {
        match target {
            MentionTarget::User { id } => named.push(id.as_uuid()),
            MentionTarget::Group { id } => groups.push(id.as_uuid()),
            MentionTarget::Channel => channel = true,
        }
    }
    if channel && !can_mention_channel(loaded.facts.workspace_role, loaded.facts.is_default) {
        return Err(AppError::forbidden(
            "only admins and owners can mention @channel in the default channel",
        ));
    }

    if !named.is_empty() {
        // People only: an integration's bot is not someone to mention.
        let found: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM workspace_members wm JOIN users u ON u.id = wm.user_id \
             WHERE wm.workspace_id = $1 AND wm.user_id = ANY($2) AND NOT u.is_bot",
        )
        .bind(loaded.workspace_id.as_uuid())
        .bind(&named)
        .fetch_one(&mut *conn)
        .await?;
        if usize::try_from(found).ok() != Some(named.len()) {
            return Err(AppError::invalid(
                "a mention names someone who is not a person in this workspace",
            ));
        }
    }
    let mut candidates = named;
    if !groups.is_empty() {
        let found: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM user_groups WHERE workspace_id = $1 AND id = ANY($2)",
        )
        .bind(loaded.workspace_id.as_uuid())
        .bind(&groups)
        .fetch_one(&mut *conn)
        .await?;
        if usize::try_from(found).ok() != Some(groups.len()) {
            return Err(AppError::invalid(
                "a mention names a group that does not exist",
            ));
        }
        let members: Vec<Uuid> =
            sqlx::query_scalar("SELECT user_id FROM user_group_members WHERE group_id = ANY($1)")
                .bind(&groups)
                .fetch_all(&mut *conn)
                .await?;
        candidates.extend(members);
    }

    // Only people who are in the conversation now, and not the author. One row per member, so the
    // list has no duplicates however often someone was named.
    let users: Vec<Uuid> = if candidates.is_empty() {
        Vec::new()
    } else {
        // A deactivated person is left out (the mention stays plain text and notifies no one).
        sqlx::query_scalar(concat!(
            "SELECT cm.user_id FROM conversation_members cm \
             JOIN workspace_members wm ON wm.workspace_id = $4 AND wm.user_id = cm.user_id AND ",
            crate::access::active_member!("wm"),
            " WHERE cm.conversation_id = $1 AND cm.user_id = ANY($2) AND cm.user_id <> $3 \
             ORDER BY cm.user_id"
        ))
        .bind(loaded.id.as_uuid())
        .bind(&candidates)
        .bind(author.as_uuid())
        .bind(loaded.workspace_id.as_uuid())
        .fetch_all(&mut *conn)
        .await?
    };
    Ok(Mentions {
        users: users.into_iter().map(UserId::from_uuid).collect(),
        channel,
    })
}

/// Records one `message_mentions` row for each person `mentions` names. `seq` is the message's own
/// position: unread is measured by message position, so an edit does not move it. `listed` says
/// whether the conversation's history lists the message (a thread-only reply's mentions count for
/// its thread, not for the conversation).
async fn store_mention_rows(
    conn: &mut PgConnection,
    message: MessageId,
    conversation: ConversationId,
    seq: EventSeq,
    listed: bool,
    mentions: &Mentions,
) -> Result<(), AppError> {
    if mentions.users.is_empty() {
        return Ok(());
    }
    let users: Vec<Uuid> = mentions.users.iter().map(|user| user.as_uuid()).collect();
    sqlx::query(
        "INSERT INTO message_mentions (message_id, user_id, conversation_id, seq, listed) \
         SELECT $1, u, $2, $3, $5 FROM unnest($4::uuid[]) AS u",
    )
    .bind(message.as_uuid())
    .bind(conversation.as_uuid())
    .bind(seq.get())
    .bind(&users)
    .bind(listed)
    .execute(conn)
    .await?;
    Ok(())
}

/// Fills in what a message carries beyond its row, each with one query for the whole slice: its
/// reactions, its attached files, and (for a root that has replies, a deleted one included) its
/// thread summary.
async fn attach_details(conn: &mut PgConnection, messages: &mut [Message]) -> Result<(), AppError> {
    attach_reactions(conn, messages).await?;
    let live: Vec<Uuid> = messages
        .iter()
        .filter(|message| !message.deleted)
        .map(|message| message.id.as_uuid())
        .collect();
    let mut files = attachments::for_messages(conn, &live).await?;
    for message in messages.iter_mut() {
        if let Some(found) = files.remove(&message.id.as_uuid()) {
            message.attachments = found;
        }
    }
    let roots: Vec<Uuid> = messages
        .iter()
        .filter(|message| message.thread_root.is_none())
        .map(|message| message.id.as_uuid())
        .collect();
    if roots.is_empty() {
        return Ok(());
    }
    let mut summaries = threads::summaries(conn, &roots).await?;
    for message in messages.iter_mut() {
        if let Some(summary) = summaries.remove(&message.id.as_uuid()) {
            message.thread = Some(summary);
        }
    }
    Ok(())
}

/// Fills in the reactions of `messages` with one query for all of them, in the order the protocol
/// defines: emoji by first use (the smallest `seq` among its current reactions), and the people
/// under each emoji by their own `seq`. Deleted messages have none and are not queried.
async fn attach_reactions(
    conn: &mut PgConnection,
    messages: &mut [Message],
) -> Result<(), AppError> {
    let ids: Vec<Uuid> = messages
        .iter()
        .filter(|message| !message.deleted)
        .map(|message| message.id.as_uuid())
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let mut by_message = reactions_of(conn, &ids).await?;
    for message in messages.iter_mut() {
        if let Some(reactions) = by_message.remove(&message.id.as_uuid()) {
            message.reactions = reactions;
        }
    }
    Ok(())
}

/// The reactions of each message in `ids` that has any. One indexed scan on
/// `message_reactions_by_message_idx` returns rows already in `(message, seq)` order, so grouping
/// them preserves the protocol's ordering without sorting.
async fn reactions_of(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<ReactionSummary>>, AppError> {
    let rows: Vec<(Uuid, String, Uuid)> = sqlx::query_as(
        "SELECT message_id, emoji, user_id FROM message_reactions \
         WHERE message_id = ANY($1) ORDER BY message_id, seq",
    )
    .bind(ids)
    .fetch_all(conn)
    .await?;
    let mut grouped: HashMap<Uuid, Vec<ReactionSummary>> = HashMap::new();
    for (message, emoji, user) in rows {
        let emoji = ReactionEmoji::parse(&emoji)
            .map_err(|_| AppError::internal("stored reaction is not an emoji"))?;
        let user = UserId::from_uuid(user);
        let summaries = grouped.entry(message).or_default();
        match summaries.iter_mut().find(|summary| summary.emoji == emoji) {
            Some(summary) => summary.users.push(user),
            None => summaries.push(ReactionSummary {
                emoji,
                users: vec![user],
            }),
        }
    }
    Ok(grouped)
}

/// What a reply needs to know about its root, read under the root's row lock.
struct RootFacts {
    author_id: Uuid,
    thread_root_id: Option<Uuid>,
    deleted: bool,
}

/// Locks the root a reply names (`FOR NO KEY UPDATE`, so replies, deletions and edits of the
/// thread take turns) if it is a message of `conversation`. Another conversation's message is
/// never locked or revealed: it is simply not found.
async fn lock_root(
    conn: &mut PgConnection,
    root: MessageId,
    conversation: ConversationId,
) -> Result<Option<RootFacts>, AppError> {
    let row: Option<(Uuid, Option<Uuid>, bool)> = sqlx::query_as(
        "SELECT author_id, thread_root_id, deleted FROM messages \
         WHERE id = $1 AND conversation_id = $2 FOR NO KEY UPDATE",
    )
    .bind(root.as_uuid())
    .bind(conversation.as_uuid())
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|(author_id, thread_root_id, deleted)| RootFacts {
        author_id,
        thread_root_id,
        deleted,
    }))
}

/// Commits a message and returns the acknowledgment.
///
/// One transaction authorizes the caller, claims the operation receipt, reserves the next event
/// position, and inserts the message and its event. The result is returned only after commit.
/// Repeating a committed operation with the same content returns the message (as it now stands,
/// so an edit or deletion since is reflected); the same operation id with different content is a
/// conflict.
///
/// The transaction holds the conversation row `FOR SHARE`: any number of sends run together, but
/// never alongside an archive or a membership change, so no message commits after an archive or
/// after its author's removal in the log. A top-level message also moves the author's read
/// position to itself, without an event: a person who has just written has read everything before
/// it. (A reply does not: it may be written from a thread pane without the channel having been
/// read, and opening a thread never moves the channel's reading position.)
///
/// A **reply** (`thread_root`) also locks the root's message row, right after the conversation and
/// before anything is written, so concurrent replies, edits and deletions of the thread take turns.
/// The root must be a top-level message of this conversation that has not been deleted (`not_found`
/// for one that is missing, in another conversation, or deleted; `invalid_request` for a reply).
/// The root's summary and the follow state of the people involved change in the same transaction,
/// and the events come in this order: `message_created`, `thread_changed`, then a
/// `thread_state_changed` for each person who started following.
pub async fn send_message(
    pool: &PgPool,
    user: UserId,
    conversation: ConversationId,
    request: SendMessageRequest,
) -> Result<Message, AppError> {
    if request.also_in_channel && request.thread_root.is_none() {
        return Err(AppError::invalid(
            "also_in_channel only applies to a reply (give a thread_root)",
        ));
    }
    if request.attachment_ids.len() > MAX_ATTACHMENTS_PER_MESSAGE {
        return Err(AppError::invalid(format!(
            "a message can have at most {MAX_ATTACHMENTS_PER_MESSAGE} files"
        )));
    }
    // Words, files, or both: a message with neither says nothing.
    if request.document.has_no_content() && request.attachment_ids.is_empty() {
        return Err(AppError::invalid("a message needs words or a file"));
    }
    let fingerprints = match request.thread_root {
        None => crate::crypto::request_fingerprints(conversation, &request.document)?,
        Some(root) => crate::crypto::reply_fingerprints(
            conversation,
            &request.document,
            root,
            request.also_in_channel,
        )?,
    };
    let fingerprints =
        crate::crypto::attachment_fingerprints(fingerprints, &request.attachment_ids);

    let (mut tx, loaded) =
        conversations::begin_visible(pool, user, conversation, Lock::Shared).await?;
    if !loaded.access().can_post() {
        return Err(AppError::forbidden("you cannot post in this conversation"));
    }
    // A reply locks its root before anything else is written (and before the receipt is claimed:
    // a retry then waits for the original, which holds it).
    let root = match request.thread_root {
        Some(root) => lock_root(&mut tx, root, conversation).await?,
        None => None,
    };

    // Claim the operation. A concurrent transaction holding the same (user, operation) makes this
    // wait for its outcome, so exactly one of two simultaneous duplicates inserts the message.
    let message_id = MessageId::generate();
    if let Some(existing) = receipts::claim(
        &mut tx,
        user,
        request.operation_id,
        ReceiptKind::SendMessage,
        &fingerprints[0],
        ReceiptTarget::Message(message_id),
    )
    .await?
    {
        existing.ensure_same_any(ReceiptKind::SendMessage, &fingerprints)?;
        let original = existing
            .message_id
            .map(MessageId::from_uuid)
            .ok_or_else(|| AppError::internal("receipt has no message"))?;
        return fetch_message(&mut tx, original)
            .await?
            .ok_or_else(|| AppError::internal("receipted message vanished"));
    }

    // A reply needs a root it can reply to. (Checked after the receipt, so a retry of a reply whose
    // root has since been deleted still returns the reply.)
    let root_author = match request.thread_root {
        None => None,
        Some(_) => {
            let root = root.ok_or_else(message_not_found)?;
            if root.deleted {
                return Err(message_not_found());
            }
            if root.thread_root_id.is_some() {
                return Err(AppError::invalid(
                    "a reply cannot be replied to: reply in the thread of its root",
                ));
            }
            let facts = MessageFacts {
                access: loaded.access(),
                is_author: root.author_id == user.as_uuid(),
                deleted: root.deleted,
            };
            if !facts.can_reply() {
                return Err(AppError::forbidden("you cannot reply in this conversation"));
            }
            Some(UserId::from_uuid(root.author_id))
        }
    };

    // Decide who this notifies (and refuse a document that cannot be sent) before reserving a position.
    let mentions = compute_mentions(&mut tx, &loaded, user, &request.document).await?;
    // The files it names must be the sender's, for this conversation, and waiting; or nothing is sent.
    let attached =
        attachments::claim_for_send(&mut tx, user, conversation, &request.attachment_ids).await?;
    // Every custom emoji in it must be live in this workspace (and stays so until it commits).
    emoji::check_document(&mut tx, loaded.workspace_id, &request.document, &[]).await?;
    let previous_newest = match request.thread_root {
        Some(root) => threads::newest_reply(&mut tx, root).await?,
        None => 0,
    };
    let (seq, now) = events::next_position(&mut tx, loaded.workspace_id).await?;
    let plain_text = request.document.plain_text();
    let listed = request.thread_root.is_none() || request.also_in_channel;
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, author_id, document, plain_text, revision, seq, created_at, \
                               mentions, mentions_channel, thread_root_id, in_channel, listed) \
         VALUES ($1, $2, $3, $4, $5, 1, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(message_id.as_uuid())
    .bind(conversation.as_uuid())
    .bind(user.as_uuid())
    .bind(Json(&request.document))
    .bind(&plain_text)
    .bind(seq.get())
    .bind(now.as_offset_date_time())
    .bind(Json(&mentions))
    .bind(mentions.channel)
    .bind(request.thread_root.map(|root| root.as_uuid()))
    .bind(request.also_in_channel)
    .bind(listed)
    .execute(&mut *tx)
    .await?;
    store_mention_rows(&mut tx, message_id, conversation, seq, listed, &mentions).await?;
    attachments::attach(&mut tx, message_id.as_uuid(), &attached).await?;
    if request.thread_root.is_none() {
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
    }

    let message = Message {
        id: message_id,
        conversation_id: conversation,
        author_id: user,
        document: request.document,
        created_at: now,
        revision: 1,
        seq,
        edited_at: None,
        thread_root: request.thread_root,
        in_channel: request.also_in_channel,
        thread: None,
        attachments: attached,
        reactions: Vec::new(),
        reactions_as_of: EventSeq::ZERO,
        mentions,
        deleted: false,
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

    if let (Some(root), Some(root_author)) = (request.thread_root, root_author) {
        // The root's summary, announced at the next position, and the people who now follow.
        let (as_of, at) = events::next_position(&mut tx, loaded.workspace_id).await?;
        let summary =
            threads::record_reply(&mut tx, root, conversation, user, seq, now, as_of).await?;
        events::insert_event(
            &mut tx,
            NewEvent {
                workspace_id: loaded.workspace_id,
                seq: as_of,
                occurred_at: at,
                kind: "thread_changed",
                conversation_id: Some(conversation),
                operation_id: None,
                audience: Audience::Members,
                event: &Event::ThreadChanged {
                    conversation_id: conversation,
                    root_id: root,
                    summary: Some(summary),
                },
            },
        )
        .await?;
        let started = threads::follow_for_reply(
            &mut tx,
            root,
            conversation,
            user,
            seq,
            previous_newest,
            root_author,
            &message.mentions.users,
        )
        .await?;
        for person in started {
            let state = threads::state_of(&mut tx, person, root, conversation)
                .await?
                .ok_or_else(|| AppError::internal("thread state vanished"))?;
            let (state_seq, state_at) = events::next_position(&mut tx, loaded.workspace_id).await?;
            events::insert_event(
                &mut tx,
                NewEvent {
                    workspace_id: loaded.workspace_id,
                    seq: state_seq,
                    occurred_at: state_at,
                    kind: "thread_state_changed",
                    conversation_id: Some(conversation),
                    operation_id: None,
                    audience: Audience::User(person),
                    event: &Event::ThreadStateChanged { state },
                },
            )
            .await?;
        }
    }
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
            .fetch_optional(&mut *conn)
            .await?;
    let Some(mut message) = row.map(MessageRow::into_message).transpose()? else {
        return Ok(None);
    };
    attach_details(conn, std::slice::from_mut(&mut message)).await?;
    Ok(Some(message))
}

/// Loads messages by id with everything they carry (reactions, files, thread summary), one query
/// for the whole set per part, the way a history page is built. Ids that do not exist are absent
/// from the result. The caller has already decided that these messages may be read (search does
/// so in the query that finds them).
pub(crate) async fn messages_by_ids(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Message>, AppError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<MessageRow> =
        sqlx::query_as(message_select!("FROM messages m WHERE m.id = ANY($1)"))
            .bind(ids)
            .fetch_all(&mut *conn)
            .await?;
    let mut messages = rows
        .into_iter()
        .map(MessageRow::into_message)
        .collect::<Result<Vec<_>, _>>()?;
    attach_details(conn, &mut messages).await?;
    Ok(messages
        .into_iter()
        .map(|message| (message.id.as_uuid(), message))
        .collect())
}

/// Pages a conversation's history, oldest first within the page. It lists what the conversation
/// lists (`messages.listed`): top-level messages and replies sent also to the channel, and deleted
/// roots that still have replies, as tombstones. Other deleted messages and thread-only replies are
/// skipped before the page is cut, so the page holds `limit` entries that belong in it and
/// `has_more` is true only if older ones remain.
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
        "FROM messages m WHERE m.conversation_id = $1 AND m.listed \
           AND ($2::bigint IS NULL OR m.seq < $2) ORDER BY m.seq DESC LIMIT $3"
    ))
    .bind(conversation.as_uuid())
    .bind(query.before.map(EventSeq::get))
    .bind(i64::from(limit) + 1)
    .fetch_all(&mut *conn)
    .await?;

    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    rows.reverse();
    let mut messages = rows
        .into_iter()
        .map(MessageRow::into_message)
        .collect::<Result<Vec<_>, _>>()?;
    // Two queries for the whole page (reactions, thread summaries), not some per message.
    attach_details(&mut conn, &mut messages).await?;
    Ok(HistoryPage { messages, has_more })
}

/// Pages the replies of a thread, oldest first within the page, like [`history`] (same query, same
/// page, `has_more` only if older replies that exist remain). The caller must be able to read the
/// conversation. A message that is a reply, that does not exist or cannot be seen, or that was
/// deleted without leaving replies is `not_found`; a deleted root that still has replies works
/// (its replies stay readable), and a top-level message nobody has replied to yet has an empty
/// thread.
pub async fn replies(
    pool: &PgPool,
    user: UserId,
    root: MessageId,
    query: HistoryQuery,
) -> Result<HistoryPage, AppError> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_HISTORY_LIMIT)
        .clamp(1, MAX_HISTORY_LIMIT);
    let mut conn = pool.acquire().await?;
    let found: Option<(Uuid, Option<Uuid>, bool)> = sqlx::query_as(
        "SELECT conversation_id, thread_root_id, deleted FROM messages WHERE id = $1",
    )
    .bind(root.as_uuid())
    .fetch_optional(&mut *conn)
    .await?;
    let Some((conversation, None, deleted)) = found else {
        return Err(message_not_found());
    };
    conversations::load(&mut conn, user, ConversationId::from_uuid(conversation))
        .await?
        .filter(conversations::Loaded::can_read)
        .ok_or_else(message_not_found)?;

    let mut rows: Vec<MessageRow> = sqlx::query_as(message_select!(
        "FROM messages m WHERE m.thread_root_id = $1 AND NOT m.deleted \
           AND ($2::bigint IS NULL OR m.seq < $2) ORDER BY m.seq DESC LIMIT $3"
    ))
    .bind(root.as_uuid())
    .bind(query.before.map(EventSeq::get))
    .bind(i64::from(limit) + 1)
    .fetch_all(&mut *conn)
    .await?;
    if deleted && rows.is_empty() && !threads::has_replies(&mut conn, root).await? {
        // Deleted, and nothing under it: gone, as from history.
        return Err(message_not_found());
    }

    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    rows.reverse();
    let mut messages = rows
        .into_iter()
        .map(MessageRow::into_message)
        .collect::<Result<Vec<_>, _>>()?;
    attach_details(&mut conn, &mut messages).await?;
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
    // What an operation produced is read as a member of its workspace: someone who is no longer an
    // active member (deactivated) is told nothing, as if there were no record.
    let workspace: Option<Uuid> = if let Some(id) = receipt.message_id {
        sqlx::query_scalar(
            "SELECT c.workspace_id FROM messages m JOIN conversations c ON c.id = m.conversation_id \
             WHERE m.id = $1",
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
    } else if let Some(id) = receipt.group_id {
        sqlx::query_scalar("SELECT workspace_id FROM user_groups WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?
    } else if let Some(id) = receipt.conversation_id {
        sqlx::query_scalar("SELECT workspace_id FROM conversations WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?
    } else {
        None
    };
    if let Some(workspace) = workspace
        && crate::access::active_role(
            &mut conn,
            lazy_domain::WorkspaceId::from_uuid(workspace),
            user,
        )
        .await?
        .is_none()
    {
        return Ok(OperationStatus::NotFound);
    }
    if let Some(id) = receipt.message_id {
        let id = MessageId::from_uuid(id);
        if receipt.kind == "edit_message" {
            let message = original_edit_result(&mut conn, id, receipt.message_revision).await?;
            return Ok(OperationStatus::EditCommitted { message });
        }
        let message = fetch_message(&mut conn, id)
            .await?
            .ok_or_else(|| AppError::internal("receipted message vanished"))?;
        return Ok(OperationStatus::Committed { message });
    }
    if receipt.group_id.is_some() {
        // A group that was created and since deleted has nothing to report.
        return Ok(
            match groups::operation_group(&mut conn, user, operation).await? {
                Some(group) => OperationStatus::GroupCommitted { group },
                None => OperationStatus::NotFound,
            },
        );
    }
    match conversations::operation_conversation(&mut conn, user, operation).await? {
        Some(conversation) => Ok(OperationStatus::ConversationCommitted { conversation }),
        None => Ok(OperationStatus::NotFound),
    }
}

// --- Reading, editing, and deleting one message --------------------------------------------

pub(crate) fn message_not_found() -> AppError {
    AppError::not_found("message not found")
}

/// One message as it stands, tombstone included. The caller must be able to read its
/// conversation; otherwise it does not exist for them.
pub async fn get_message(pool: &PgPool, user: UserId, id: MessageId) -> Result<Message, AppError> {
    let mut conn = pool.acquire().await?;
    let message = fetch_message(&mut conn, id)
        .await?
        .ok_or_else(message_not_found)?;
    conversations::load(&mut conn, user, message.conversation_id)
        .await?
        .filter(conversations::Loaded::can_read)
        .ok_or_else(message_not_found)?;
    Ok(message)
}

/// Starts the transaction for changing a message: finds its conversation (which never changes),
/// then holds that conversation row `FOR SHARE` like a send does, so the change cannot interleave
/// with an archive or a removal. A message the caller cannot see is `not_found`.
async fn begin_message_change(
    pool: &PgPool,
    actor: UserId,
    id: MessageId,
) -> Result<(Transaction<'static, Postgres>, conversations::Loaded), AppError> {
    let conversation: Option<Uuid> =
        sqlx::query_scalar("SELECT conversation_id FROM messages WHERE id = $1")
            .bind(id.as_uuid())
            .fetch_optional(pool)
            .await?;
    let conversation = conversation
        .map(ConversationId::from_uuid)
        .ok_or_else(message_not_found)?;
    conversations::begin_visible(pool, actor, conversation, Lock::Shared)
        .await
        .map_err(|error| match error {
            AppError::Api(api) if api.code == ErrorCode::NotFound => message_not_found(),
            other => other,
        })
}

/// Locks the message row (`FOR NO KEY UPDATE`, the lock an `UPDATE` takes, which still lets other
/// transactions' foreign-key checks proceed) and returns it. Two changes to one message exclude
/// each other here.
async fn lock_message(
    conn: &mut PgConnection,
    id: MessageId,
) -> Result<Option<MessageRow>, AppError> {
    let row = sqlx::query_as(message_select!(
        "FROM messages m WHERE m.id = $1 FOR NO KEY UPDATE OF m"
    ))
    .bind(id.as_uuid())
    .fetch_optional(conn)
    .await?;
    Ok(row)
}

/// Edits a message.
///
/// The request is an operation like a send: it claims a receipt, so the same operation id with
/// the same content returns its **original** result even after the message has moved on, and the
/// same id with different content is a `conflict`. A new operation is checked in this order, each
/// failure leaving nothing behind: a deleted or invisible message is `not_found`; someone other
/// than the author, or an author who can no longer post there, is `forbidden`; a `base_revision`
/// that is not the current revision is `stale_revision`. An edit that changes nothing commits only
/// its receipt, returns the current message, and appends no event.
pub async fn edit_message(
    pool: &PgPool,
    actor: UserId,
    id: MessageId,
    request: EditMessageRequest,
) -> Result<Message, AppError> {
    let document_json = serde_json::to_vec(&request.document)
        .map_err(|e| AppError::internal(format!("cannot serialize document: {e}")))?;
    let fingerprint = crate::crypto::command_fingerprint(
        "edit_message",
        &[
            id.as_uuid().as_bytes(),
            &request.base_revision.to_be_bytes(),
            &document_json,
        ],
    );

    let (mut tx, loaded) = begin_message_change(pool, actor, id).await?;
    if let Some(existing) = receipts::claim(
        &mut tx,
        actor,
        request.operation_id,
        ReceiptKind::EditMessage,
        &fingerprint,
        ReceiptTarget::Message(id),
    )
    .await?
    {
        existing.ensure_same(ReceiptKind::EditMessage, &fingerprint)?;
        return original_edit_result(&mut tx, id, existing.message_revision).await;
    }

    let row = lock_message(&mut tx, id)
        .await?
        .filter(|row| !row.deleted)
        .ok_or_else(message_not_found)?;
    let facts = MessageFacts {
        access: loaded.access(),
        is_author: row.author_id == actor.as_uuid(),
        deleted: row.deleted,
    };
    if !facts.can_edit() {
        return Err(AppError::forbidden("you cannot edit this message"));
    }
    let current = u32::try_from(row.revision)
        .map_err(|_| AppError::internal("stored message revision is negative"))?;
    if request.base_revision != current {
        return Err(AppError::new(
            ErrorCode::StaleRevision,
            "the message has been edited since; fetch the current version",
        ));
    }

    if row.document.0 == request.document {
        // Nothing to commit but the receipt, which answers a later status query.
        receipts::set_message_revision(&mut tx, actor, request.operation_id, current).await?;
        let mut message = row.into_message()?;
        attach_details(&mut tx, std::slice::from_mut(&mut message)).await?;
        tx.commit().await?;
        return Ok(message);
    }

    // Words can be taken out of a message only if it has files to say what is left.
    if request.document.has_no_content() {
        let has_files: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM attachments WHERE message_id = $1 AND deleted_at IS NULL)",
        )
        .bind(id.as_uuid())
        .fetch_one(&mut *tx)
        .await?;
        if !has_files {
            return Err(AppError::invalid("a message without files needs words"));
        }
    }
    // The emoji the edit adds must be live; those the message already had are not checked again,
    // so an edit is not refused because an emoji it already contained has been deleted since.
    emoji::check_document(
        &mut tx,
        loaded.workspace_id,
        &request.document,
        &row.document.0.custom_emoji(),
    )
    .await?;
    // The edit may add or remove mentions. Resolve them (and refuse what cannot be sent) while
    // nothing has been written, then reserve the position as the last step.
    let mentions = compute_mentions(&mut tx, &loaded, actor, &request.document).await?;
    let previous_mentions = row.mentions.0.clone();
    let (seq, now) = events::next_position(&mut tx, loaded.workspace_id).await?;
    sqlx::query(
        "INSERT INTO message_revisions (message_id, revision, author_id, document, edited_at, mentions) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id.as_uuid())
    .bind(row.revision)
    .bind(row.author_id)
    .bind(Json(&row.document.0))
    .bind(row.edited_at)
    .bind(Json(&previous_mentions))
    .execute(&mut *tx)
    .await?;
    let revision = current + 1;
    sqlx::query(
        "UPDATE messages SET document = $2, plain_text = $3, revision = $4, edited_at = $5, \
                mentions = $6, mentions_channel = $7 \
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(Json(&request.document))
    .bind(request.document.plain_text())
    .bind(i32::try_from(revision).unwrap_or(i32::MAX))
    .bind(now.as_offset_date_time())
    .bind(Json(&mentions))
    .bind(mentions.channel)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM message_mentions WHERE message_id = $1")
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await?;
    store_mention_rows(
        &mut tx,
        id,
        loaded.id,
        EventSeq::new(row.seq).unwrap_or(EventSeq::ZERO),
        row.is_listed(),
        &mentions,
    )
    .await?;
    receipts::set_message_revision(&mut tx, actor, request.operation_id, revision).await?;

    let mut message = Message {
        document: request.document,
        revision,
        edited_at: Some(now),
        mentions,
        ..row.into_message()?
    };
    attach_details(&mut tx, std::slice::from_mut(&mut message)).await?;
    events::insert_event(
        &mut tx,
        NewEvent {
            workspace_id: loaded.workspace_id,
            seq,
            occurred_at: now,
            kind: "message_edited",
            conversation_id: Some(message.conversation_id),
            operation_id: Some(request.operation_id),
            audience: Audience::Members,
            event: &Event::MessageEdited {
                message: message.clone(),
                operation_id: Some(request.operation_id),
                previous_mentions,
            },
        },
    )
    .await?;
    tx.commit().await?;
    Ok(message)
}

/// The result an edit operation originally returned: the message at the revision the edit produced
/// (or found). If the message has moved on that revision is read back from the kept revisions; if
/// it has been deleted since, those are gone and the tombstone is the only honest answer.
async fn original_edit_result(
    conn: &mut PgConnection,
    id: MessageId,
    revision: Option<i32>,
) -> Result<Message, AppError> {
    let current = fetch_message(conn, id)
        .await?
        .ok_or_else(|| AppError::internal("receipted message vanished"))?;
    let Some(revision) = revision else {
        return Ok(current);
    };
    if u32::try_from(revision).ok() == Some(current.revision) {
        return Ok(current);
    }
    // (author, document, edited_at, mentions) of that revision.
    type Prior = (
        Uuid,
        Json<MessageDocument>,
        Option<OffsetDateTime>,
        Json<Mentions>,
    );
    let prior: Option<Prior> = sqlx::query_as(
        "SELECT author_id, document, edited_at, mentions FROM message_revisions \
             WHERE message_id = $1 AND revision = $2",
    )
    .bind(id.as_uuid())
    .bind(revision)
    .fetch_optional(conn)
    .await?;
    Ok(match prior {
        Some((author, document, edited_at, mentions)) => Message {
            author_id: UserId::from_uuid(author),
            document: document.0,
            mentions: mentions.0,
            revision: u32::try_from(revision).unwrap_or(current.revision),
            edited_at: edited_at.map(Timestamp::from),
            deleted: false,
            ..current
        },
        None => current,
    })
}

/// Deletes a message: the author's own, or anyone's if the caller is an admin or owner who can
/// read the conversation (see [`MessageFacts::can_delete`]). Idempotent: deleting a deleted message
/// returns the tombstone, to anyone who could have deleted it.
///
/// The message becomes a tombstone with a higher revision. Everything that held its content is
/// cleared in the same transaction: the prior revisions are deleted, and the message's earlier
/// `message_created` and `message_edited` events have their message replaced by the tombstone, so
/// neither a replay nor the database keeps the text. (This is the one place events are rewritten.)
/// Only the `message` inside those events changes: `previous_mentions` on a rewritten
/// `message_edited` stays as written (user ids, no content), and `message_deleted.had_mentions` is
/// the mentions of the last version, the stored set at the time of deletion, so a client that
/// counted any of these as a mention can undo exactly what it counted.
///
/// Threads. Deleting a **reply** removes it from its thread and from the channel if it was also
/// sent there (`listed`), takes it out of the root's summary, and announces that as a
/// `thread_changed` after the `message_deleted`: the new summary, or `None` when no replies remain
/// (and then a deleted root with nothing under it leaves history too). Deleting a **root that has
/// replies** leaves a tombstone that is still listed and keeps its summary (the `message_deleted`
/// carries it), so the replies stay readable; a root without replies is removed as before.
pub async fn delete_message(
    pool: &PgPool,
    actor: UserId,
    id: MessageId,
) -> Result<Message, AppError> {
    let (mut tx, loaded) = begin_message_change(pool, actor, id).await?;
    // A reply's root is locked before the reply, the order a send uses.
    let root_of: Option<Uuid> =
        sqlx::query_scalar("SELECT thread_root_id FROM messages WHERE id = $1")
            .bind(id.as_uuid())
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
    let root_deleted = match root_of {
        Some(root) => Some(
            sqlx::query_scalar::<_, bool>(
                "SELECT deleted FROM messages WHERE id = $1 FOR NO KEY UPDATE",
            )
            .bind(root)
            .fetch_one(&mut *tx)
            .await?,
        ),
        None => None,
    };
    let row = lock_message(&mut tx, id)
        .await?
        .ok_or_else(message_not_found)?;
    let permitted = MessageFacts {
        access: loaded.access(),
        is_author: row.author_id == actor.as_uuid(),
        deleted: false,
    }
    .can_delete();
    if !permitted {
        return Err(AppError::forbidden("you cannot delete this message"));
    }
    if row.deleted {
        let mut message = row.into_message()?;
        attach_details(&mut tx, std::slice::from_mut(&mut message)).await?;
        return Ok(message);
    }

    let reply_root = row.thread_root_id.map(MessageId::from_uuid);
    // A root's summary stays on its tombstone while replies remain.
    let kept_summary = match reply_root {
        None => threads::summary_of(&mut tx, id).await?,
        Some(_) => None,
    };
    let (seq, now) = events::next_position(&mut tx, loaded.workspace_id).await?;
    let placeholder = deleted_placeholder();
    let revision = row.revision + 1;
    // Who it notified, for the event: a client that counted this message as a mention for someone
    // stops counting it without needing the message.
    let had_mentions = row.mentions.0.clone();
    sqlx::query("DELETE FROM message_revisions WHERE message_id = $1")
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM message_mentions WHERE message_id = $1")
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM message_reactions WHERE message_id = $1")
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await?;
    // Its files can no longer be read, from this moment; their bytes follow in the cleanup job.
    attachments::mark_message_deleted(&mut tx, id.as_uuid()).await?;
    sqlx::query(
        "UPDATE messages SET document = $2, plain_text = '', revision = $3, edited_at = NULL, \
                reactions_as_of = $4, mentions = '{}'::jsonb, mentions_channel = false, \
                listed = $5, deleted = true \
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(Json(&placeholder))
    .bind(revision)
    .bind(seq.get())
    .bind(kept_summary.is_some())
    .execute(&mut *tx)
    .await?;
    let mut tombstone = Message {
        document: placeholder,
        revision: u32::try_from(revision).unwrap_or(u32::MAX),
        edited_at: None,
        // Clearing the reactions is a change to them, made at this position, so a client that
        // keeps the highest `reactions_as_of` ends with none.
        reactions: Vec::new(),
        reactions_as_of: seq,
        // A tombstone notifies nobody.
        mentions: Mentions::default(),
        deleted: true,
        ..row.into_message()?
    };
    // The earlier events become this tombstone, without the thread summary: they do not describe
    // the thread, and its events (`thread_changed`) carry it.
    let tombstone_json = serde_json::to_value(&tombstone)
        .map_err(|e| AppError::internal(format!("cannot serialize message: {e}")))?;
    sqlx::query(
        "UPDATE events SET payload = jsonb_set(payload, '{message}', $2) \
         WHERE message_id = $1 AND kind IN ('message_created', 'message_edited')",
    )
    .bind(id.as_uuid())
    .bind(Json(tombstone_json))
    .execute(&mut *tx)
    .await?;
    tombstone.thread = kept_summary;

    events::insert_event(
        &mut tx,
        NewEvent {
            workspace_id: loaded.workspace_id,
            seq,
            occurred_at: now,
            kind: "message_deleted",
            conversation_id: Some(tombstone.conversation_id),
            operation_id: None,
            audience: Audience::Members,
            event: &Event::MessageDeleted {
                message: tombstone.clone(),
                had_mentions,
            },
        },
    )
    .await?;

    if let Some(root) = reply_root {
        let (as_of, at) = events::next_position(&mut tx, loaded.workspace_id).await?;
        let summary = threads::remove_reply(&mut tx, root, as_of).await?;
        if summary.is_none() && root_deleted == Some(true) {
            // The last reply under a deleted root: nothing is left of the thread, so the tombstone
            // leaves history and nobody has state for it any more.
            sqlx::query("UPDATE messages SET listed = false WHERE id = $1")
                .bind(root.as_uuid())
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM thread_user_state WHERE root_id = $1")
                .bind(root.as_uuid())
                .execute(&mut *tx)
                .await?;
        }
        events::insert_event(
            &mut tx,
            NewEvent {
                workspace_id: loaded.workspace_id,
                seq: as_of,
                occurred_at: at,
                kind: "thread_changed",
                conversation_id: Some(tombstone.conversation_id),
                operation_id: None,
                audience: Audience::Members,
                event: &Event::ThreadChanged {
                    conversation_id: tombstone.conversation_id,
                    root_id: root,
                    summary,
                },
            },
        )
        .await?;
    }
    tx.commit().await?;
    Ok(tombstone)
}

/// Sets whether `actor` has reacted to a message with one emoji.
///
/// This sets a state, so it is idempotent: asking for what already holds changes nothing, appends
/// no event, and reserves no position; the message is returned as it stands. Checks run in this
/// order, each failure leaving nothing behind: a message that is deleted or that the caller cannot
/// read is `not_found`; one they can read but could not post in (a conversation they have not
/// joined, or an archived one) is `forbidden`; adding a *new* emoji to a message that already has
/// [`MAX_REACTION_KINDS`] different ones is `limit_reached`. Removing a reaction and adding it back
/// is always fine, because removing the last of a kind frees its place.
///
/// It locks the same rows in the same order as an edit (the conversation row, then the message
/// row, then the position), so a reaction, an edit, and a deletion of one message take turns. That
/// is also what makes the limit exact: nothing can add a kind between the count and the insert.
///
/// The change inserts or deletes one `message_reactions` row stamped with the new position, sets
/// the message's `reactions_as_of` to it, and appends `reactions_changed` carrying the complete new
/// list.
pub async fn set_reaction(
    pool: &PgPool,
    actor: UserId,
    id: MessageId,
    request: SetReactionRequest,
) -> Result<Message, AppError> {
    let (mut tx, loaded) = begin_message_change(pool, actor, id).await?;
    let row = lock_message(&mut tx, id)
        .await?
        .filter(|row| !row.deleted)
        .ok_or_else(message_not_found)?;
    let facts = MessageFacts {
        access: loaded.access(),
        is_author: row.author_id == actor.as_uuid(),
        deleted: row.deleted,
    };
    if !facts.can_react() {
        return Err(AppError::forbidden("you cannot react in this conversation"));
    }

    let emoji = request.emoji.to_string();
    let emoji = emoji.as_str();
    let held: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM message_reactions \
                        WHERE message_id = $1 AND user_id = $2 AND emoji = $3)",
    )
    .bind(id.as_uuid())
    .bind(actor.as_uuid())
    .bind(emoji)
    .fetch_one(&mut *tx)
    .await?;
    let mut message = row.into_message()?;
    if held == request.present {
        // Already so: nothing to commit. The transaction is dropped and rolls back.
        attach_details(&mut tx, std::slice::from_mut(&mut message)).await?;
        return Ok(message);
    }
    if request.present {
        // A custom emoji can be newly used only while it is live (withdrawing needs no check).
        emoji::check_reaction(&mut tx, loaded.workspace_id, request.emoji).await?;
        let (kinds, has_kind): (i64, bool) = sqlx::query_as(
            "SELECT count(DISTINCT emoji), COALESCE(bool_or(emoji = $2), false) \
             FROM message_reactions WHERE message_id = $1",
        )
        .bind(id.as_uuid())
        .bind(emoji)
        .fetch_one(&mut *tx)
        .await?;
        if !has_kind && usize::try_from(kinds).unwrap_or(usize::MAX) >= MAX_REACTION_KINDS {
            return Err(AppError::new(
                ErrorCode::LimitReached,
                format!("a message can have at most {MAX_REACTION_KINDS} different reactions"),
            ));
        }
    }

    // Something changes, so this is the point to reserve the position.
    let (seq, now) = events::next_position(&mut tx, loaded.workspace_id).await?;
    if request.present {
        sqlx::query(
            "INSERT INTO message_reactions (message_id, user_id, emoji, seq) VALUES ($1, $2, $3, $4)",
        )
        .bind(id.as_uuid())
        .bind(actor.as_uuid())
        .bind(emoji)
        .bind(seq.get())
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "DELETE FROM message_reactions WHERE message_id = $1 AND user_id = $2 AND emoji = $3",
        )
        .bind(id.as_uuid())
        .bind(actor.as_uuid())
        .bind(emoji)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE messages SET reactions_as_of = $2 WHERE id = $1")
        .bind(id.as_uuid())
        .bind(seq.get())
        .execute(&mut *tx)
        .await?;
    message.reactions_as_of = seq;
    attach_details(&mut tx, std::slice::from_mut(&mut message)).await?;

    events::insert_event(
        &mut tx,
        NewEvent {
            workspace_id: loaded.workspace_id,
            seq,
            occurred_at: now,
            kind: "reactions_changed",
            conversation_id: Some(message.conversation_id),
            operation_id: None,
            audience: Audience::Members,
            event: &Event::ReactionsChanged {
                conversation_id: message.conversation_id,
                message_id: id,
                reactions: message.reactions.clone(),
            },
        },
    )
    .await?;
    tx.commit().await?;
    Ok(message)
}
