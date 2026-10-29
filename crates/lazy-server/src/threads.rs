//! Threads: the summary a root carries, and each person's follow and read state for a thread.
//! (Replies themselves are messages; sending, deleting and paging them is in [`crate::messages`].)
//!
//! # The summary
//!
//! A root with replies has one row in `threads`: the number of replies that still exist, the
//! position and time of the newest, up to five recent repliers, and `as_of`, the position of the
//! latest change. It is **maintained in the transaction that adds or deletes a reply**, which holds
//! the root's message row, so concurrent repliers take turns and the summary is exact. Adding a
//! reply is O(1). Deleting one recomputes the newest reply (an index lookup) and the repliers (an
//! index scan from the newest reply back, which stops at the fifth different person).
//!
//! # Following and reading
//!
//! `thread_user_state` has a row for each person who follows or has unfollowed a thread; a thread
//! they have no row for is one they have not touched. A reply makes its author follow (even after an
//! unfollow), and makes the root's author and each person it mentions by name follow unless they
//! chose to stop. The **counts are not stored**: unread is the replies from others after the
//! person's read position, mentions are those of them that name the person or use `@channel`, and
//! both are one index range scan of `messages_replies_idx` over only what is unread (measured in
//! llm-docs/SERVER.md). Nothing has to be adjusted when a reply is edited or deleted.
//!
//! Locks are taken in the order the rest of the server uses: the conversation row, the root's
//! message row, the person's state row, and the workspace row for the event position last.

use std::collections::HashMap;

use lazy_domain::{
    ConversationId, EventSeq, MAX_THREAD_REPLIERS_LISTED, MessageId, Timestamp, UserId, WorkspaceId,
};
use lazy_protocol::{Event, ThreadSummary, ThreadUserState};
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::conversations::{self, Lock};
use crate::error::AppError;
use crate::events::{self, Audience, PendingEvent};
use crate::messages::message_not_found;

/// How many threads the bootstrap lists for one person.
pub const MAX_BOOTSTRAP_THREADS: i64 = 500;

#[derive(sqlx::FromRow)]
struct SummaryRow {
    root_id: Uuid,
    reply_count: i32,
    last_reply_seq: i64,
    last_reply_at: OffsetDateTime,
    recent_repliers: Vec<Uuid>,
    as_of: i64,
}

// The SQL below hard-codes the cap (`[1:5]` and the table's CHECK).
const _: () = assert!(MAX_THREAD_REPLIERS_LISTED == 5);

impl SummaryRow {
    fn into_summary(self) -> Result<(Uuid, ThreadSummary), AppError> {
        let seq = |value: i64| {
            EventSeq::new(value).ok_or_else(|| AppError::internal("stored position is negative"))
        };
        Ok((
            self.root_id,
            ThreadSummary {
                reply_count: u32::try_from(self.reply_count)
                    .map_err(|_| AppError::internal("stored reply count is negative"))?,
                last_reply_seq: seq(self.last_reply_seq)?,
                last_reply_at: Timestamp::from(self.last_reply_at),
                recent_repliers: self
                    .recent_repliers
                    .into_iter()
                    .map(UserId::from_uuid)
                    .collect(),
                as_of: seq(self.as_of)?,
            },
        ))
    }
}

/// The summaries of those of `roots` that have replies, with one query.
pub(crate) async fn summaries(
    conn: &mut PgConnection,
    roots: &[Uuid],
) -> Result<HashMap<Uuid, ThreadSummary>, AppError> {
    if roots.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<SummaryRow> = sqlx::query_as(
        "SELECT root_id, reply_count, last_reply_seq, last_reply_at, recent_repliers, as_of \
         FROM threads WHERE root_id = ANY($1)",
    )
    .bind(roots)
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(SummaryRow::into_summary).collect()
}

/// The summary of one root, if it has replies.
pub(crate) async fn summary_of(
    conn: &mut PgConnection,
    root: MessageId,
) -> Result<Option<ThreadSummary>, AppError> {
    Ok(summaries(conn, &[root.as_uuid()])
        .await?
        .remove(&root.as_uuid()))
}

/// Whether a root has replies that have not been deleted.
pub(crate) async fn has_replies(
    conn: &mut PgConnection,
    root: MessageId,
) -> Result<bool, AppError> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM threads WHERE root_id = $1)")
            .bind(root.as_uuid())
            .fetch_one(conn)
            .await?,
    )
}

/// The position of the newest reply to `root` that has not been deleted, or 0.
pub(crate) async fn newest_reply(
    conn: &mut PgConnection,
    root: MessageId,
) -> Result<i64, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(max(seq), 0) FROM messages WHERE thread_root_id = $1 AND NOT deleted",
    )
    .bind(root.as_uuid())
    .fetch_one(conn)
    .await?)
}

/// Adds a reply to its root's summary and returns the summary as it now stands. `as_of` is the
/// position of the `ThreadChanged` event that will announce it. The caller holds the root row.
pub(crate) async fn record_reply(
    conn: &mut PgConnection,
    root: MessageId,
    conversation: ConversationId,
    replier: UserId,
    reply_seq: EventSeq,
    reply_at: Timestamp,
    as_of: EventSeq,
) -> Result<ThreadSummary, AppError> {
    let row: SummaryRow = sqlx::query_as(
        "INSERT INTO threads (root_id, conversation_id, reply_count, last_reply_seq, last_reply_at, \
                              recent_repliers, as_of) \
         VALUES ($1, $2, 1, $3, $4, ARRAY[$5::uuid], $6) \
         ON CONFLICT (root_id) DO UPDATE SET \
             reply_count = threads.reply_count + 1, \
             last_reply_seq = excluded.last_reply_seq, \
             last_reply_at = excluded.last_reply_at, \
             recent_repliers = (ARRAY[$5::uuid] || array_remove(threads.recent_repliers, $5::uuid))[1:5], \
             as_of = excluded.as_of \
         RETURNING root_id, reply_count, last_reply_seq, last_reply_at, recent_repliers, as_of",
    )
    .bind(root.as_uuid())
    .bind(conversation.as_uuid())
    .bind(reply_seq.get())
    .bind(reply_at.as_offset_date_time())
    .bind(replier.as_uuid())
    .bind(as_of.get())
    .fetch_one(conn)
    .await?;
    Ok(row.into_summary()?.1)
}

/// Takes a deleted reply out of its root's summary and returns the summary as it now stands, or
/// `None` if no replies remain (the row is removed). The reply row must already be marked deleted,
/// and the caller holds the root row.
pub(crate) async fn remove_reply(
    conn: &mut PgConnection,
    root: MessageId,
    as_of: EventSeq,
) -> Result<Option<ThreadSummary>, AppError> {
    let count: Option<i32> =
        sqlx::query_scalar("SELECT reply_count FROM threads WHERE root_id = $1 FOR UPDATE")
            .bind(root.as_uuid())
            .fetch_optional(&mut *conn)
            .await?;
    let Some(count) = count else {
        return Err(AppError::internal("a reply's thread has no summary"));
    };
    if count <= 1 {
        sqlx::query("DELETE FROM threads WHERE root_id = $1")
            .bind(root.as_uuid())
            .execute(&mut *conn)
            .await?;
        return Ok(None);
    }
    let (last_seq, last_at): (i64, OffsetDateTime) = sqlx::query_as(
        "SELECT seq, created_at FROM messages \
         WHERE thread_root_id = $1 AND NOT deleted ORDER BY seq DESC LIMIT 1",
    )
    .bind(root.as_uuid())
    .fetch_one(&mut *conn)
    .await?;
    let repliers = recent_repliers(conn, root).await?;
    let row: SummaryRow = sqlx::query_as(
        "UPDATE threads SET reply_count = reply_count - 1, last_reply_seq = $2, last_reply_at = $3, \
                recent_repliers = $4, as_of = $5 \
         WHERE root_id = $1 \
         RETURNING root_id, reply_count, last_reply_seq, last_reply_at, recent_repliers, as_of",
    )
    .bind(root.as_uuid())
    .bind(last_seq)
    .bind(last_at)
    .bind(&repliers)
    .bind(as_of.get())
    .fetch_one(conn)
    .await?;
    Ok(Some(row.into_summary()?.1))
}

/// The different people who wrote the newest replies that still exist, most recent first, at most
/// [`MAX_THREAD_REPLIERS_LISTED`]. Reads back from the newest reply in pages and stops at the
/// fifth person, so a long thread of a few people costs only as many rows as it takes to find them.
async fn recent_repliers(conn: &mut PgConnection, root: MessageId) -> Result<Vec<Uuid>, AppError> {
    const PAGE: i64 = 100;
    let mut found: Vec<Uuid> = Vec::with_capacity(MAX_THREAD_REPLIERS_LISTED);
    let mut before = i64::MAX;
    loop {
        let rows: Vec<(Uuid, i64)> = sqlx::query_as(
            "SELECT author_id, seq FROM messages \
             WHERE thread_root_id = $1 AND NOT deleted AND seq < $2 ORDER BY seq DESC LIMIT $3",
        )
        .bind(root.as_uuid())
        .bind(before)
        .bind(PAGE)
        .fetch_all(&mut *conn)
        .await?;
        for (author, seq) in &rows {
            before = *seq;
            if !found.contains(author) {
                found.push(*author);
                if found.len() == MAX_THREAD_REPLIERS_LISTED {
                    return Ok(found);
                }
            }
        }
        if rows.len() < PAGE as usize {
            return Ok(found);
        }
    }
}

// --- A person's state ------------------------------------------------------------------------

/// Unread replies and unread replies that mention `user`, for a thread and a read position.
///
/// One range scan of `messages_replies_idx` from the read position: replies from other people that
/// have not been deleted; of those, the ones that use `@channel` or have a `message_mentions` row
/// for the person (a primary-key probe, made only for replies that are actually unread).
pub(crate) async fn counts(
    conn: &mut PgConnection,
    user: UserId,
    root: MessageId,
    last_read: i64,
) -> Result<(u32, u32), AppError> {
    let (unread, mentions): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE r.author_id <> $3), \
                count(*) FILTER (WHERE r.author_id <> $3 AND (r.mentions_channel OR EXISTS \
                    (SELECT 1 FROM message_mentions mm WHERE mm.message_id = r.id AND mm.user_id = $3))) \
         FROM messages r WHERE r.thread_root_id = $1 AND r.seq > $2 AND NOT r.deleted",
    )
    .bind(root.as_uuid())
    .bind(last_read)
    .bind(user.as_uuid())
    .fetch_one(conn)
    .await?;
    Ok((
        u32::try_from(unread).unwrap_or(u32::MAX),
        u32::try_from(mentions).unwrap_or(u32::MAX),
    ))
}

fn seq_from(value: i64) -> Result<EventSeq, AppError> {
    EventSeq::new(value).ok_or_else(|| AppError::internal("stored position is negative"))
}

async fn state_from(
    conn: &mut PgConnection,
    user: UserId,
    root: MessageId,
    conversation: ConversationId,
    following: bool,
    last_read: i64,
) -> Result<ThreadUserState, AppError> {
    let (unread_count, mention_count) = counts(conn, user, root, last_read).await?;
    Ok(ThreadUserState {
        root_id: root,
        conversation_id: conversation,
        following,
        last_read: seq_from(last_read)?,
        unread_count,
        mention_count,
    })
}

/// A person's stored state for a thread, with its counts, or `None` if they have none.
pub(crate) async fn state_of(
    conn: &mut PgConnection,
    user: UserId,
    root: MessageId,
    conversation: ConversationId,
) -> Result<Option<ThreadUserState>, AppError> {
    let row: Option<(bool, i64)> = sqlx::query_as(
        "SELECT following, last_read FROM thread_user_state WHERE user_id = $1 AND root_id = $2",
    )
    .bind(user.as_uuid())
    .bind(root.as_uuid())
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        Some((following, last_read)) => Ok(Some(
            state_from(conn, user, root, conversation, following, last_read).await?,
        )),
        None => Ok(None),
    }
}

/// Applies the follow rules for a new reply and returns the people whose state this created or
/// turned on (they are told with a `ThreadStateChanged`).
///
/// * The replier follows, even after unfollowing, and has read the thread up to their reply.
/// * The root's author, when someone else replies, and each person the reply mentions by name,
///   follow unless they chose to stop. A person new to the thread starts having read everything
///   before this reply (`previous_newest`), so only this reply is unread.
///
/// The caller holds the root row, so nobody else changes these rows meanwhile.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn follow_for_reply(
    conn: &mut PgConnection,
    root: MessageId,
    conversation: ConversationId,
    replier: UserId,
    reply_seq: EventSeq,
    previous_newest: i64,
    root_author: UserId,
    mentioned: &[UserId],
) -> Result<Vec<UserId>, AppError> {
    let mut changed = Vec::new();

    let before: Option<bool> = sqlx::query_scalar(
        "SELECT following FROM thread_user_state WHERE user_id = $1 AND root_id = $2 FOR UPDATE",
    )
    .bind(replier.as_uuid())
    .bind(root.as_uuid())
    .fetch_optional(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO thread_user_state (user_id, root_id, conversation_id, following, unfollowed, last_read) \
         VALUES ($1, $2, $3, true, false, $4) \
         ON CONFLICT (user_id, root_id) DO UPDATE SET \
             following = true, unfollowed = false, \
             last_read = GREATEST(thread_user_state.last_read, excluded.last_read)",
    )
    .bind(replier.as_uuid())
    .bind(root.as_uuid())
    .bind(conversation.as_uuid())
    .bind(reply_seq.get())
    .execute(&mut *conn)
    .await?;
    if before != Some(true) {
        changed.push(replier);
    }

    let mut others: Vec<Uuid> = mentioned.iter().map(|user| user.as_uuid()).collect();
    others.push(root_author.as_uuid());
    others.retain(|user| *user != replier.as_uuid());
    others.sort();
    others.dedup();
    if !others.is_empty() {
        // Rows that already follow, or chose not to, are left alone and so are not returned.
        let turned_on: Vec<Uuid> = sqlx::query_scalar(
            "INSERT INTO thread_user_state (user_id, root_id, conversation_id, following, unfollowed, last_read) \
             SELECT u, $2, $3, true, false, $4 FROM unnest($1::uuid[]) AS u \
             ON CONFLICT (user_id, root_id) DO UPDATE SET following = true \
                 WHERE NOT thread_user_state.following AND NOT thread_user_state.unfollowed \
             RETURNING user_id",
        )
        .bind(&others)
        .bind(root.as_uuid())
        .bind(conversation.as_uuid())
        .bind(previous_newest)
        .fetch_all(&mut *conn)
        .await?;
        let mut turned_on: Vec<UserId> = turned_on.into_iter().map(UserId::from_uuid).collect();
        turned_on.sort();
        changed.extend(turned_on);
    }
    Ok(changed)
}

/// The threads to list in a person's bootstrap for one workspace: those they follow and those with
/// replies they have not read, in conversations they are in, newest activity first, at most
/// [`MAX_BOOTSTRAP_THREADS`].
///
/// The candidates are chosen from the summaries alone (following, or the newest reply is past their
/// read position) and cut to the limit before any counting, so the cost is bounded by the limit
/// however many threads the person has touched. Counting then runs once per candidate against its
/// replies index; a candidate that turns out to have nothing unread and is not followed is dropped.
pub(crate) async fn user_threads(
    conn: &mut PgConnection,
    user: UserId,
    workspace: WorkspaceId,
) -> Result<Vec<ThreadUserState>, AppError> {
    let rows: Vec<(Uuid, Uuid, bool, i64, i64, i64)> = sqlx::query_as(
        "WITH candidates AS ( \
             SELECT s.root_id, s.conversation_id, s.following, s.last_read, t.last_reply_seq \
             FROM thread_user_state s \
             JOIN threads t ON t.root_id = s.root_id \
             JOIN conversation_members cm \
                  ON cm.conversation_id = s.conversation_id AND cm.user_id = s.user_id \
             JOIN conversations c ON c.id = s.conversation_id AND c.workspace_id = $2 \
             WHERE s.user_id = $1 AND (s.following OR s.last_read < t.last_reply_seq) \
             ORDER BY t.last_reply_seq DESC, s.root_id LIMIT $3 \
         ) \
         SELECT c.root_id, c.conversation_id, c.following, c.last_read, k.unread, k.mentions \
         FROM candidates c \
         CROSS JOIN LATERAL ( \
             SELECT count(*) FILTER (WHERE r.author_id <> $1) AS unread, \
                    count(*) FILTER (WHERE r.author_id <> $1 AND (r.mentions_channel OR EXISTS \
                        (SELECT 1 FROM message_mentions mm WHERE mm.message_id = r.id AND mm.user_id = $1))) AS mentions \
             FROM messages r WHERE r.thread_root_id = c.root_id AND r.seq > c.last_read AND NOT r.deleted \
         ) k \
         WHERE c.following OR k.unread > 0 \
         ORDER BY c.last_reply_seq DESC, c.root_id",
    )
    .bind(user.as_uuid())
    .bind(workspace.as_uuid())
    .bind(MAX_BOOTSTRAP_THREADS)
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(
            |(root, conversation, following, last_read, unread, mentions)| {
                Ok(ThreadUserState {
                    root_id: MessageId::from_uuid(root),
                    conversation_id: ConversationId::from_uuid(conversation),
                    following,
                    last_read: seq_from(last_read)?,
                    unread_count: u32::try_from(unread).unwrap_or(u32::MAX),
                    mention_count: u32::try_from(mentions).unwrap_or(u32::MAX),
                })
            },
        )
        .collect()
}

// --- The commands ----------------------------------------------------------------------------

/// Starts a transaction for changing the caller's state for the thread of `root`: finds the
/// message's conversation (which never changes), holds that conversation `FOR SHARE`, then the
/// root's message row `FOR SHARE`, so the change cannot interleave with a reply, an archive, or a
/// removal. A message that is not a root, is not visible, or is a deleted root with no replies is
/// `not_found`; someone who can read the conversation but is not in it is `forbidden`.
async fn begin_thread_change(
    pool: &PgPool,
    actor: UserId,
    root: MessageId,
) -> Result<
    (
        sqlx::Transaction<'static, sqlx::Postgres>,
        conversations::Loaded,
    ),
    AppError,
> {
    let found: Option<(Uuid, Option<Uuid>)> =
        sqlx::query_as("SELECT conversation_id, thread_root_id FROM messages WHERE id = $1")
            .bind(root.as_uuid())
            .fetch_optional(pool)
            .await?;
    let conversation = match found {
        Some((conversation, None)) => ConversationId::from_uuid(conversation),
        _ => return Err(message_not_found()),
    };
    let (mut tx, loaded) = conversations::begin_visible(pool, actor, conversation, Lock::Shared)
        .await
        .map_err(|error| match error {
            AppError::Api(api) if api.code == lazy_protocol::ErrorCode::NotFound => {
                message_not_found()
            }
            other => other,
        })?;
    let deleted: Option<bool> =
        sqlx::query_scalar("SELECT deleted FROM messages WHERE id = $1 FOR SHARE")
            .bind(root.as_uuid())
            .fetch_optional(&mut *tx)
            .await?;
    let deleted = deleted.ok_or_else(message_not_found)?;
    if deleted {
        let has_replies: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM threads WHERE root_id = $1)")
                .bind(root.as_uuid())
                .fetch_one(&mut *tx)
                .await?;
        if !has_replies {
            return Err(message_not_found());
        }
    }
    if !loaded.facts.is_member {
        return Err(AppError::forbidden(
            "you cannot change your state for a thread in a conversation you are not in",
        ));
    }
    Ok((tx, loaded))
}

async fn announce(
    tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
    loaded: &conversations::Loaded,
    user: UserId,
    state: &ThreadUserState,
) -> Result<(), AppError> {
    events::append_all(
        tx,
        loaded.workspace_id,
        vec![PendingEvent {
            kind: "thread_state_changed",
            conversation_id: Some(loaded.id),
            operation_id: None,
            audience: Audience::User(user),
            event: Event::ThreadStateChanged {
                state: state.clone(),
            },
        }],
    )
    .await?;
    Ok(())
}

/// Sets whether the caller follows a thread. This sets a state, so it is idempotent: asking for
/// what already holds changes nothing, appends no event, and reserves no position.
///
/// A person with no state for the thread starts at its newest reply (nothing unread). Unfollowing
/// is remembered even then, so a later mention does not bring them back; since a person with no
/// state is not following either, that is not a change anyone is told about.
pub async fn set_follow(
    pool: &PgPool,
    actor: UserId,
    root: MessageId,
    following: bool,
) -> Result<ThreadUserState, AppError> {
    let (mut tx, loaded) = begin_thread_change(pool, actor, root).await?;
    let existing: Option<(bool, bool, i64)> = sqlx::query_as(
        "SELECT following, unfollowed, last_read FROM thread_user_state \
         WHERE user_id = $1 AND root_id = $2 FOR UPDATE",
    )
    .bind(actor.as_uuid())
    .bind(root.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    let (changed, last_read) = match existing {
        None => {
            let newest = newest_reply(&mut tx, root).await?;
            sqlx::query(
                "INSERT INTO thread_user_state (user_id, root_id, conversation_id, following, unfollowed, last_read) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(actor.as_uuid())
            .bind(root.as_uuid())
            .bind(loaded.id.as_uuid())
            .bind(following)
            .bind(!following)
            .bind(newest)
            .execute(&mut *tx)
            .await?;
            (following, newest)
        }
        Some((was_following, was_unfollowed, last_read)) => {
            if was_following != following || was_unfollowed == following {
                sqlx::query(
                    "UPDATE thread_user_state SET following = $3, unfollowed = $4 \
                     WHERE user_id = $1 AND root_id = $2",
                )
                .bind(actor.as_uuid())
                .bind(root.as_uuid())
                .bind(following)
                .bind(!following)
                .execute(&mut *tx)
                .await?;
            }
            (was_following != following, last_read)
        }
    };
    let state = state_from(&mut tx, actor, root, loaded.id, following, last_read).await?;
    if changed {
        announce(&mut tx, &loaded, actor, &state).await?;
    }
    tx.commit().await?;
    Ok(state)
}

/// Moves the caller's read position in a thread forward to `up_to`, clamped to the thread's newest
/// reply. A position at or behind the current one changes nothing: no write, no event, no position.
///
/// A person with no state for the thread has none to move: as far as the server is concerned they
/// have not been following it, and the answer is the thread read to its newest reply.
pub async fn mark_read(
    pool: &PgPool,
    actor: UserId,
    root: MessageId,
    up_to: EventSeq,
) -> Result<ThreadUserState, AppError> {
    let (mut tx, loaded) = begin_thread_change(pool, actor, root).await?;
    let newest = newest_reply(&mut tx, root).await?;
    let existing: Option<(bool, i64)> = sqlx::query_as(
        "SELECT following, last_read FROM thread_user_state \
         WHERE user_id = $1 AND root_id = $2 FOR UPDATE",
    )
    .bind(actor.as_uuid())
    .bind(root.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    let Some((following, last_read)) = existing else {
        return state_from(&mut tx, actor, root, loaded.id, false, newest).await;
    };
    let target = up_to.get().min(newest);
    let advanced = target > last_read;
    let last_read = if advanced {
        sqlx::query(
            "UPDATE thread_user_state SET last_read = $3 WHERE user_id = $1 AND root_id = $2",
        )
        .bind(actor.as_uuid())
        .bind(root.as_uuid())
        .bind(target)
        .execute(&mut *tx)
        .await?;
        target
    } else {
        last_read
    };
    let state = state_from(&mut tx, actor, root, loaded.id, following, last_read).await?;
    if advanced {
        announce(&mut tx, &loaded, actor, &state).await?;
    }
    tx.commit().await?;
    Ok(state)
}
