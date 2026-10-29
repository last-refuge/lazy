//! Notification settings, conversation preferences, status, and the thread inbox (llm-docs/
//! DECISIONS.md, D20).
//!
//! The server **stores and syncs** these and serves the inbox; it does not decide who a message
//! notifies. That is `lazy_domain::notification::decide`, evaluated by each client for the messages
//! it receives (and later by a push worker), so a message to a large channel writes nothing per
//! recipient.
//!
//! * Settings are per person and workspace, replaced whole, idempotent; an identical request commits
//!   nothing and appends no event. A change is announced to the person only
//!   (`NotificationSettingsChanged`, `Audience::User`).
//! * A conversation preference is per person and conversation; the defaults are no row. Announced to
//!   the person only, with no conversation on the event, so a person who can read a public channel
//!   without being in it still hears of their own change.
//! * A status is per person and workspace; announced to the whole workspace. It is never announced
//!   when it expires: a status past `expires_at` is served as none (the database clock at read time).
//! * The inbox lists the threads a person follows, newest activity first, and marks them read.

use std::collections::HashMap;

use lazy_domain::notification::{
    ConversationNotificationPrefs, NotificationSettings, NotifyLevel, UserStatus,
};
use lazy_domain::{
    ConversationId, EventSeq, MessageId, ReactionEmoji, Timestamp, UserId, WorkspaceId,
    WorkspaceRole,
};
use lazy_protocol::{
    Event, InboxEntry, InboxPage, InboxQuery, InboxReadResponse, ThreadUserState, User,
};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::conversations::{self, Lock};
use crate::error::AppError;
use crate::events::{self, Audience, PendingEvent};
use crate::{db, emoji, messages};

/// Furthest ahead `dnd_until` may be set.
const MAX_SNOOZE_DAYS: i32 = 366;
/// Inbox page size when the request does not say, and the most it may ask for.
pub const DEFAULT_INBOX_LIMIT: u32 = 20;
pub const MAX_INBOX_LIMIT: u32 = 50;
/// Most threads one `POST .../inbox/read` marks.
pub const MAX_INBOX_READ: i64 = 500;

async fn require_member(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    user: UserId,
) -> Result<WorkspaceRole, AppError> {
    crate::access::require_member(conn, workspace, user).await
}

/// One advisory lock per (kind, person, workspace), held to the end of the transaction, so two
/// devices changing the same thing take turns and their events are appended in the order the
/// changes were written.
async fn advisory(conn: &mut PgConnection, name: String) -> Result<(), AppError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(name)
        .execute(conn)
        .await?;
    Ok(())
}

// --- Settings ------------------------------------------------------------------------------

/// The person's settings for the workspace: what they stored, else the defaults.
pub(crate) async fn settings_of(
    conn: &mut PgConnection,
    user: UserId,
    workspace: WorkspaceId,
) -> Result<NotificationSettings, AppError> {
    let stored: Option<Json<NotificationSettings>> = sqlx::query_scalar(
        "SELECT settings FROM notification_settings WHERE user_id = $1 AND workspace_id = $2",
    )
    .bind(user.as_uuid())
    .bind(workspace.as_uuid())
    .fetch_optional(conn)
    .await?;
    Ok(stored.map(|json| json.0).unwrap_or_default())
}

/// Replaces the caller's settings for a workspace.
///
/// A non-member is `not_found`. Quiet hours must satisfy `QuietHours::is_valid`, and `dnd_until`
/// may be at most 366 days ahead (`invalid_request`); a `dnd_until` already past is accepted and
/// **stored as given**: it means no snooze (`is_paused` compares it with the time), and storing it
/// unchanged keeps a repeated request identical, whatever the time. Settings equal to what is in
/// force (stored, or the defaults when nothing is stored) commit nothing and append no event;
/// settings equal to the defaults delete the row.
pub async fn set_settings(
    pool: &PgPool,
    actor: UserId,
    workspace: WorkspaceId,
    settings: NotificationSettings,
) -> Result<NotificationSettings, AppError> {
    let mut tx = pool.begin().await?;
    require_member(&mut tx, workspace, actor).await?;
    if let Some(hours) = &settings.quiet_hours
        && !hours.is_valid()
    {
        return Err(AppError::invalid(
            "quiet hours need different start and end minutes within a day, at least one day, \
             and a UTC offset within 14 hours",
        ));
    }
    if let Some(until) = settings.dnd_until {
        let too_far: bool =
            sqlx::query_scalar("SELECT $1::timestamptz > now() + make_interval(days => $2)")
                .bind(until.as_offset_date_time())
                .bind(MAX_SNOOZE_DAYS)
                .fetch_one(&mut *tx)
                .await?;
        if too_far {
            return Err(AppError::invalid(format!(
                "notifications can be paused for at most {MAX_SNOOZE_DAYS} days"
            )));
        }
    }
    advisory(&mut tx, format!("notify-settings:{actor}:{workspace}")).await?;
    let current = settings_of(&mut tx, actor, workspace).await?;
    if current == settings {
        return Ok(settings);
    }
    if settings == NotificationSettings::default() {
        sqlx::query("DELETE FROM notification_settings WHERE user_id = $1 AND workspace_id = $2")
            .bind(actor.as_uuid())
            .bind(workspace.as_uuid())
            .execute(&mut *tx)
            .await?;
    } else {
        sqlx::query(
            "INSERT INTO notification_settings (user_id, workspace_id, settings) VALUES ($1, $2, $3) \
             ON CONFLICT (user_id, workspace_id) DO UPDATE SET settings = excluded.settings, updated_at = now()",
        )
        .bind(actor.as_uuid())
        .bind(workspace.as_uuid())
        .bind(Json(&settings))
        .execute(&mut *tx)
        .await?;
    }
    events::append_all(
        &mut tx,
        workspace,
        vec![PendingEvent {
            kind: "notification_settings_changed",
            conversation_id: None,
            operation_id: None,
            audience: Audience::User(actor),
            event: Event::NotificationSettingsChanged { settings },
        }],
    )
    .await?;
    tx.commit().await?;
    Ok(settings)
}

// --- Conversation preferences --------------------------------------------------------------

fn level_to_db(level: NotifyLevel) -> &'static str {
    match level {
        NotifyLevel::All => "all",
        NotifyLevel::Mentions => "mentions",
        NotifyLevel::Nothing => "nothing",
    }
}

fn level_from_db(text: &str) -> Result<NotifyLevel, AppError> {
    match text {
        "all" => Ok(NotifyLevel::All),
        "mentions" => Ok(NotifyLevel::Mentions),
        "nothing" => Ok(NotifyLevel::Nothing),
        other => Err(AppError::internal(format!(
            "stored notification level {other:?} is unknown"
        ))),
    }
}

/// The caller's preferences for the conversations of `workspace` that they can read (those they
/// are in and public channels), in conversation order.
pub(crate) async fn prefs_in_workspace(
    conn: &mut PgConnection,
    user: UserId,
    workspace: WorkspaceId,
) -> Result<Vec<ConversationNotificationPrefs>, AppError> {
    let rows: Vec<(Uuid, Option<String>, bool)> = sqlx::query_as(
        "SELECT p.conversation_id, p.level, p.muted \
         FROM conversation_notification_prefs p JOIN conversations c ON c.id = p.conversation_id \
         WHERE p.user_id = $1 AND c.workspace_id = $2 \
           AND ((c.kind = 'channel' AND c.visibility = 'public') OR EXISTS \
                (SELECT 1 FROM conversation_members cm \
                 WHERE cm.conversation_id = c.id AND cm.user_id = p.user_id)) \
         ORDER BY p.conversation_id",
    )
    .bind(user.as_uuid())
    .bind(workspace.as_uuid())
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|(conversation, level, muted)| {
            Ok(ConversationNotificationPrefs {
                conversation_id: ConversationId::from_uuid(conversation),
                level: level.as_deref().map(level_from_db).transpose()?,
                muted,
            })
        })
        .collect()
}

/// Sets the caller's preference for one conversation: a level of its own and/or a mute. The caller
/// must be able to read the conversation (a member, or anyone in the workspace for a public
/// channel; `not_found` otherwise, as for any conversation they cannot see). A preference with
/// neither deletes the row. Idempotent: setting what is already in force commits nothing. The row
/// is also deleted when the person leaves or is removed.
pub async fn set_prefs(
    pool: &PgPool,
    actor: UserId,
    conversation: ConversationId,
    level: Option<NotifyLevel>,
    muted: bool,
) -> Result<ConversationNotificationPrefs, AppError> {
    // Shared: a removal (which deletes the row) takes the conversation exclusively, so it cannot
    // interleave with this write.
    let (mut tx, loaded) =
        conversations::begin_visible(pool, actor, conversation, Lock::Shared).await?;
    let prefs = ConversationNotificationPrefs {
        conversation_id: conversation,
        level,
        muted,
    };
    let changed = if level.is_none() && !muted {
        sqlx::query(
            "DELETE FROM conversation_notification_prefs WHERE user_id = $1 AND conversation_id = $2",
        )
        .bind(actor.as_uuid())
        .bind(conversation.as_uuid())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0
    } else {
        // One statement: it writes only when something differs, and concurrent callers take turns
        // on the row.
        sqlx::query_scalar::<_, i32>(
            "INSERT INTO conversation_notification_prefs AS p (user_id, conversation_id, level, muted) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (user_id, conversation_id) DO UPDATE SET level = excluded.level, muted = excluded.muted \
             WHERE (p.level, p.muted) IS DISTINCT FROM (excluded.level, excluded.muted) RETURNING 1",
        )
        .bind(actor.as_uuid())
        .bind(conversation.as_uuid())
        .bind(level.map(level_to_db))
        .bind(muted)
        .fetch_optional(&mut *tx)
        .await?
        .is_some()
    };
    if changed {
        events::append_all(
            &mut tx,
            loaded.workspace_id,
            vec![PendingEvent {
                kind: "conversation_notification_prefs_changed",
                // No conversation: the event is the person's own and must reach them even when
                // they only read a public channel and are not in it.
                conversation_id: None,
                operation_id: None,
                audience: Audience::User(actor),
                event: Event::ConversationNotificationPrefsChanged { prefs },
            }],
        )
        .await?;
    }
    tx.commit().await?;
    Ok(prefs)
}

// --- Status --------------------------------------------------------------------------------

pub(crate) fn status_from_row(
    text: &str,
    emoji: Option<&str>,
    expires_at: Option<OffsetDateTime>,
) -> Result<UserStatus, AppError> {
    let emoji = emoji
        .map(ReactionEmoji::parse)
        .transpose()
        .map_err(|_| AppError::internal("stored status emoji is invalid"))?;
    UserStatus::new(text, emoji, expires_at.map(Timestamp::from))
        .map_err(|_| AppError::internal("stored status is invalid"))
}

/// Sets or clears the caller's status in a workspace and returns them as others see them.
///
/// A non-member is `not_found`, a bot `forbidden`. An `expires_at` already past is
/// `invalid_request`; a custom emoji must be live in this workspace (`invalid_request`; checked
/// under a share lock on the emoji's row, so deleting it and using it take turns). The end is kept
/// to the microsecond the database stores, so the same request twice is the same status. Setting
/// what is in force (an expired status counts as none) commits nothing and appends no event;
/// otherwise `UserStatusChanged` goes to the whole workspace.
pub async fn set_status(
    pool: &PgPool,
    actor: UserId,
    workspace: WorkspaceId,
    status: Option<UserStatus>,
) -> Result<User, AppError> {
    let mut tx = pool.begin().await?;
    require_member(&mut tx, workspace, actor).await?;
    // Shared, so the profile the event carries is the person's current one: a profile change
    // (which changes the row) either commits before this reads it or waits for this to finish.
    let user_row: (String, String, bool, i32) = sqlx::query_as(
        "SELECT username, display_name, is_bot, avatar_version FROM users WHERE id = $1 FOR SHARE",
    )
    .bind(actor.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    if user_row.2 {
        return Err(AppError::forbidden("an app cannot have a status"));
    }
    let status = match status {
        Some(mut status) => {
            if let Some(until) = status.expires_at {
                // The precision the database keeps (microseconds), so a repeat compares equal.
                let at = until.as_offset_date_time();
                let micros = at.nanosecond() / 1000 * 1000;
                status.expires_at = Some(Timestamp::from(
                    at.replace_nanosecond(micros)
                        .map_err(|_| AppError::invalid("that end time is not valid"))?,
                ));
                let past: bool = sqlx::query_scalar("SELECT $1::timestamptz <= now()")
                    .bind(status.expires_at.map(|t| t.as_offset_date_time()))
                    .fetch_one(&mut *tx)
                    .await?;
                if past {
                    return Err(AppError::invalid(
                        "that status has already ended; choose a time in the future",
                    ));
                }
            }
            if let Some(emoji) = status.emoji {
                emoji::check_reaction(&mut tx, workspace, emoji).await?;
            }
            Some(status)
        }
        None => None,
    };
    advisory(&mut tx, format!("status:{actor}:{workspace}")).await?;
    let stored: Option<(String, Option<String>, Option<OffsetDateTime>, bool)> = sqlx::query_as(
        "SELECT text, emoji, expires_at, (expires_at IS NULL OR expires_at > now()) AS active \
         FROM user_status WHERE user_id = $1 AND workspace_id = $2",
    )
    .bind(actor.as_uuid())
    .bind(workspace.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    let in_force = match &stored {
        Some((text, emoji, expires_at, true)) => {
            Some(status_from_row(text, emoji.as_deref(), *expires_at)?)
        }
        _ => None,
    };
    let (username, display_name, is_bot, avatar_version) = user_row;
    let mut user = db::user_from_db(
        actor.as_uuid(),
        &username,
        &display_name,
        is_bot,
        avatar_version,
    )?;
    user.status = status.clone();
    if in_force == status {
        if status.is_none() && stored.is_some() {
            // An expired status nobody sees any more: tidy it up quietly.
            sqlx::query("DELETE FROM user_status WHERE user_id = $1 AND workspace_id = $2")
                .bind(actor.as_uuid())
                .bind(workspace.as_uuid())
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        return Ok(user);
    }
    match &status {
        None => {
            sqlx::query("DELETE FROM user_status WHERE user_id = $1 AND workspace_id = $2")
                .bind(actor.as_uuid())
                .bind(workspace.as_uuid())
                .execute(&mut *tx)
                .await?;
        }
        Some(status) => {
            sqlx::query(
                "INSERT INTO user_status (user_id, workspace_id, text, emoji, expires_at) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (user_id, workspace_id) DO UPDATE \
                 SET text = excluded.text, emoji = excluded.emoji, expires_at = excluded.expires_at, \
                     updated_at = now()",
            )
            .bind(actor.as_uuid())
            .bind(workspace.as_uuid())
            .bind(status.text())
            .bind(status.emoji.map(|emoji| emoji.to_string()))
            .bind(status.expires_at.map(|t| t.as_offset_date_time()))
            .execute(&mut *tx)
            .await?;
        }
    }
    events::append_all(
        &mut tx,
        workspace,
        vec![PendingEvent {
            kind: "user_status_changed",
            conversation_id: None,
            operation_id: None,
            audience: Audience::Workspace,
            event: Event::UserStatusChanged {
                user_id: actor,
                status,
            },
        }],
    )
    .await?;
    tx.commit().await?;
    Ok(user)
}

// --- The inbox -----------------------------------------------------------------------------

type InboxRow = (Uuid, Uuid, i64, i64, i64, i64);

/// The inbox statement: the page of followed threads is chosen first, from the thread summaries and
/// the person's state alone (walking `threads_activity_idx` from the newest and probing their state,
/// which is cheap whatever the cursor), and only then are the unread and mention counts taken, once
/// per thread of the page.
macro_rules! inbox_sql {
    ($filter:literal) => {
        concat!(
            "WITH page AS ( \
                 SELECT s.root_id, s.conversation_id, s.last_read, t.last_reply_seq \
                 FROM thread_user_state s \
                 JOIN threads t ON t.root_id = s.root_id \
                 JOIN conversation_members cm \
                      ON cm.conversation_id = s.conversation_id AND cm.user_id = s.user_id \
                 JOIN conversations c ON c.id = s.conversation_id AND c.workspace_id = $2 \
                 WHERE s.user_id = $1 AND s.following AND ($3::bigint IS NULL OR t.last_reply_seq < $3) ",
            $filter,
            "ORDER BY t.last_reply_seq DESC, s.root_id LIMIT $4) \
             SELECT p.root_id, p.conversation_id, p.last_read, p.last_reply_seq, k.unread, k.mentions \
             FROM page p CROSS JOIN LATERAL ( \
                 SELECT count(*) FILTER (WHERE r.author_id <> $1) AS unread, \
                        count(*) FILTER (WHERE r.author_id <> $1 AND (r.mentions_channel OR EXISTS \
                            (SELECT 1 FROM message_mentions mm WHERE mm.message_id = r.id AND mm.user_id = $1))) AS mentions \
                 FROM messages r WHERE r.thread_root_id = p.root_id AND r.seq > p.last_read AND NOT r.deleted \
             ) k \
             ORDER BY p.last_reply_seq DESC, p.root_id"
        )
    };
}

/// The followed threads of `user` in `workspace`, most recently active first.
///
/// Candidates are the person's `thread_user_state` rows with `following` whose conversation is in
/// the workspace and whose person is still a member, joined to `threads` (so a deleted root with no
/// replies left is not there; a tombstoned root with replies is), cut by the cursor (`last_reply_seq`
/// below `before`) and by the limit. With `unread`, only threads with a reply from someone else,
/// not deleted, past the person's read position. The counts come with the rows, from one lateral
/// count per candidate; the messages, last replies and conversations of the page are then loaded
/// with the batch loaders history uses, so a page costs a fixed number of queries.
pub async fn inbox(
    pool: &PgPool,
    user: UserId,
    workspace: WorkspaceId,
    query: InboxQuery,
) -> Result<InboxPage, AppError> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_INBOX_LIMIT)
        .clamp(1, MAX_INBOX_LIMIT) as usize;
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    require_member(&mut tx, workspace, user).await?;
    let unread_only = query.unread.unwrap_or(false);
    // Two literal statements (never built from input). Not prepared: a cached generic plan could
    // not know the cursor, and the plan depends on it.
    let statement = if unread_only {
        inbox_sql!(
            "AND s.last_read < t.last_reply_seq AND EXISTS (SELECT 1 FROM messages r \
             WHERE r.thread_root_id = s.root_id AND r.seq > s.last_read AND NOT r.deleted \
               AND r.author_id <> $1) "
        )
    } else {
        inbox_sql!("")
    };
    let mut rows: Vec<InboxRow> = sqlx::query_as(statement)
        .persistent(false)
        .bind(user.as_uuid())
        .bind(workspace.as_uuid())
        .bind(query.before.map(EventSeq::get))
        .bind(limit as i64 + 1)
        .fetch_all(&mut *tx)
        .await?;
    let has_more = rows.len() > limit;
    rows.truncate(limit);

    let roots: Vec<Uuid> = rows.iter().map(|row| row.0).collect();
    // The newest reply that still exists, per thread, by the replies index read backwards.
    let last_replies: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT r.root, lr.id FROM unnest($1::uuid[]) AS r(root) \
         CROSS JOIN LATERAL (SELECT m.id FROM messages m \
                             WHERE m.thread_root_id = r.root AND NOT m.deleted \
                             ORDER BY m.seq DESC LIMIT 1) lr",
    )
    .bind(&roots)
    .fetch_all(&mut *tx)
    .await?;
    let last_reply_of: HashMap<Uuid, Uuid> = last_replies.into_iter().collect();
    let mut wanted = roots.clone();
    wanted.extend(last_reply_of.values().copied());
    let mut found = messages::messages_by_ids(&mut tx, &wanted).await?;
    let mut conversation_ids: Vec<Uuid> = rows.iter().map(|row| row.1).collect();
    conversation_ids.sort();
    conversation_ids.dedup();
    let conversations = conversations::conversations_by_ids(&mut tx, &conversation_ids).await?;
    tx.commit().await?;

    let mut entries = Vec::with_capacity(rows.len());
    for (root, conversation, last_read, _last_reply_seq, unread, mentions) in rows {
        let Some(root_message) = found.get(&root).cloned() else {
            continue;
        };
        let Some(conversation_dto) = conversations.get(&conversation).cloned() else {
            continue;
        };
        let last_reply = last_reply_of
            .get(&root)
            .and_then(|id| found.get(id))
            .cloned();
        entries.push(InboxEntry {
            root: root_message,
            last_reply,
            conversation: conversation_dto,
            state: ThreadUserState {
                root_id: MessageId::from_uuid(root),
                conversation_id: ConversationId::from_uuid(conversation),
                following: true,
                last_read: EventSeq::new(last_read)
                    .ok_or_else(|| AppError::internal("stored position is negative"))?,
                unread_count: u32::try_from(unread).unwrap_or(u32::MAX),
                mention_count: u32::try_from(mentions).unwrap_or(u32::MAX),
            },
        });
    }
    found.clear();
    Ok(InboxPage { entries, has_more })
}

/// Marks the caller's followed threads that have unread replies read, up to each one's newest
/// reply, at most [`MAX_INBOX_READ`] (the most recently active first), in one transaction, and
/// announces each to the caller (`ThreadStateChanged`). Returns how many it updated; ask again for
/// more. A thread whose newest reply arrives while this runs stays unread (only the reply that
/// existed when the thread was chosen is marked).
pub async fn mark_inbox_read(
    pool: &PgPool,
    user: UserId,
    workspace: WorkspaceId,
) -> Result<InboxReadResponse, AppError> {
    let mut tx = pool.begin().await?;
    require_member(&mut tx, workspace, user).await?;
    let chosen: Vec<(Uuid, Uuid, i64)> = sqlx::query_as(
        "SELECT s.root_id, s.conversation_id, t.last_reply_seq \
         FROM thread_user_state s \
         JOIN threads t ON t.root_id = s.root_id \
         JOIN conversation_members cm \
              ON cm.conversation_id = s.conversation_id AND cm.user_id = s.user_id \
         JOIN conversations c ON c.id = s.conversation_id AND c.workspace_id = $2 \
         WHERE s.user_id = $1 AND s.following AND s.last_read < t.last_reply_seq \
           AND EXISTS (SELECT 1 FROM messages r WHERE r.thread_root_id = s.root_id \
                       AND r.seq > s.last_read AND NOT r.deleted AND r.author_id <> $1) \
         ORDER BY t.last_reply_seq DESC, s.root_id LIMIT $3 FOR UPDATE OF s",
    )
    .bind(user.as_uuid())
    .bind(workspace.as_uuid())
    .bind(MAX_INBOX_READ)
    .fetch_all(&mut *tx)
    .await?;
    if chosen.is_empty() {
        return Ok(InboxReadResponse { updated: 0 });
    }
    let roots: Vec<Uuid> = chosen.iter().map(|row| row.0).collect();
    let targets: Vec<i64> = chosen.iter().map(|row| row.2).collect();
    sqlx::query(
        "UPDATE thread_user_state s SET last_read = u.target \
         FROM unnest($2::uuid[], $3::bigint[]) AS u(root, target) \
         WHERE s.user_id = $1 AND s.root_id = u.root",
    )
    .bind(user.as_uuid())
    .bind(&roots)
    .bind(&targets)
    .execute(&mut *tx)
    .await?;
    let announcements: Vec<PendingEvent> = chosen
        .iter()
        .map(|(root, conversation, newest)| {
            Ok(PendingEvent {
                kind: "thread_state_changed",
                conversation_id: Some(ConversationId::from_uuid(*conversation)),
                operation_id: None,
                audience: Audience::User(user),
                event: Event::ThreadStateChanged {
                    state: ThreadUserState {
                        root_id: MessageId::from_uuid(*root),
                        conversation_id: ConversationId::from_uuid(*conversation),
                        following: true,
                        last_read: EventSeq::new(*newest)
                            .ok_or_else(|| AppError::internal("stored position is negative"))?,
                        // Everything up to the newest reply was marked; a reply that arrives after
                        // this transaction is a later event.
                        unread_count: 0,
                        mention_count: 0,
                    },
                },
            })
        })
        .collect::<Result<_, AppError>>()?;
    events::append_all(&mut tx, workspace, announcements).await?;
    tx.commit().await?;
    Ok(InboxReadResponse {
        updated: u32::try_from(chosen.len()).unwrap_or(u32::MAX),
    })
}

/// Everything about a person's attention that a bootstrap carries for one workspace.
pub(crate) async fn bootstrap_parts(
    conn: &mut PgConnection,
    user: UserId,
    workspace: WorkspaceId,
) -> Result<(NotificationSettings, Vec<ConversationNotificationPrefs>), AppError> {
    Ok((
        settings_of(conn, user, workspace).await?,
        prefs_in_workspace(conn, user, workspace).await?,
    ))
}
