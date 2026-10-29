//! Discovery (llm-docs/DECISIONS.md, D21): the people directory and profiles, and saved messages.
//! The channel directory is `conversations::browse_channels`.
//!
//! * **People** are paged by display name (case-insensitive), then username, then id; integration
//!   bots are left out of the list but have a profile. A query is a case-insensitive substring match
//!   done with `strpos(lower(..), lower($query))`, so `%`, `_` and `\` in it are plain characters.
//! * **Saved messages** are a per-person set with an order the server assigns when a message is
//!   saved (`saved_order`, an identity column: larger is later; re-saving keeps it, unsaving and
//!   saving again gets a new one). The list and the bootstrap show only messages that are not
//!   deleted and whose conversation the person can read, by the same access reading the message
//!   has (a workspace member who is in the conversation or it is a public channel); an item that
//!   is unreadable now comes back when it is readable again, since the row stays.
//!   `SavedChanged` goes to the person alone, once per real change.

use lazy_domain::{ConversationId, MessageId, Timestamp, UserId, WorkspaceId, WorkspaceRole};
use lazy_protocol::{
    DEFAULT_DIRECTORY_PAGE, Event, MAX_DIRECTORY_PAGE, MAX_DIRECTORY_QUERY_CHARS, PeoplePage,
    PeopleQuery, Person, SAVED_BOOTSTRAP_LIMIT, SavedItem, SavedPage, SavedQuery, SavedResponse,
};
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::conversations::{self, Lock};
use crate::error::AppError;
use crate::events::{self, Audience, PendingEvent};
use crate::{db, messages, notifications};

/// How far into a directory a request may page; later results are reached by narrowing.
pub const MAX_DIRECTORY_OFFSET: u32 = 100_000;

async fn require_member(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    user: UserId,
) -> Result<WorkspaceRole, AppError> {
    crate::access::require_member(conn, workspace, user).await
}

/// The paging and text of a directory request, checked: the text trimmed (and `None` when empty),
/// at most [`MAX_DIRECTORY_QUERY_CHARS`] characters; the offset at most [`MAX_DIRECTORY_OFFSET`];
/// the limit clamped to `1..=MAX_DIRECTORY_PAGE` (default [`DEFAULT_DIRECTORY_PAGE`]).
pub(crate) fn directory_paging(
    query: Option<&str>,
    offset: Option<u32>,
    limit: Option<u32>,
) -> Result<(Option<String>, i64, usize), AppError> {
    // Text cannot hold a NUL; a person cannot type one, but a client can send one.
    let text = query.map(|q| q.replace('\0', " "));
    let text = text
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_owned);
    if let Some(text) = &text
        && text.chars().count() > MAX_DIRECTORY_QUERY_CHARS
    {
        return Err(AppError::invalid(format!(
            "a search can be at most {MAX_DIRECTORY_QUERY_CHARS} characters"
        )));
    }
    let offset = offset.unwrap_or(0);
    if offset > MAX_DIRECTORY_OFFSET {
        return Err(AppError::invalid(format!(
            "a directory can be paged at most {MAX_DIRECTORY_OFFSET} results in; narrow it with a search"
        )));
    }
    let limit = limit
        .unwrap_or(DEFAULT_DIRECTORY_PAGE)
        .clamp(1, MAX_DIRECTORY_PAGE) as usize;
    Ok((text, i64::from(offset), limit))
}

// --- People ----------------------------------------------------------------------------------

type PersonRow = (
    Uuid,
    String,
    String,
    bool,
    String,
    OffsetDateTime,
    Option<String>,
    Option<String>,
    Option<OffsetDateTime>,
    bool,
    i32,
);

fn person_from_row(row: PersonRow) -> Result<Person, AppError> {
    let (
        id,
        username,
        display_name,
        is_bot,
        role,
        joined_at,
        text,
        emoji,
        expires_at,
        deactivated,
        avatar_version,
    ) = row;
    let mut user = db::user_from_db(id, &username, &display_name, is_bot, avatar_version)?;
    user.deactivated = deactivated;
    if let Some(text) = text {
        user.status = Some(notifications::status_from_row(
            &text,
            emoji.as_deref(),
            expires_at,
        )?);
    }
    Ok(Person {
        user,
        role: db::role_from_db(&role)?,
        joined_at: Timestamp::from(joined_at),
    })
}

/// The columns and joins both people statements share: a workspace member with their user row and
/// their status as it stands now (one that has ended is not served).
macro_rules! person_select {
    ($tail:literal) => {
        concat!(
            "SELECT u.id, u.username, u.display_name, u.is_bot, wm.role, wm.joined_at, \
                    us.text, us.emoji, us.expires_at, (wm.deactivated_at IS NOT NULL) AS deactivated, \
                    u.avatar_version \
             FROM workspace_members wm JOIN users u ON u.id = wm.user_id \
             LEFT JOIN user_status us ON us.user_id = u.id AND us.workspace_id = wm.workspace_id \
                  AND (us.expires_at IS NULL OR us.expires_at > now()) ",
            $tail
        )
    };
}

/// One page of the workspace's people (not integration bots), by display name, username, id.
/// Deactivated people are left out unless an admin or owner asks for them with
/// `include_deactivated` (anyone else asking is `forbidden`).
pub async fn people(
    pool: &PgPool,
    actor: UserId,
    workspace: WorkspaceId,
    query: PeopleQuery,
) -> Result<PeoplePage, AppError> {
    let mut conn = pool.acquire().await?;
    let role = require_member(&mut conn, workspace, actor).await?;
    let include_deactivated = query.include_deactivated.unwrap_or(false);
    if include_deactivated && !role.can_manage_workspace() {
        return Err(AppError::forbidden(
            "only admins and owners can list deactivated people",
        ));
    }
    let (text, offset, limit) =
        directory_paging(query.query.as_deref(), query.offset, query.limit)?;
    let mut rows: Vec<PersonRow> = sqlx::query_as(person_select!(
        "WHERE wm.workspace_id = $1 AND NOT u.is_bot \
           AND ($2::text IS NULL OR strpos(lower(u.display_name), lower($2)) > 0 \
                OR strpos(lower(u.username), lower($2)) > 0) \
           AND ($5 OR wm.deactivated_at IS NULL) \
         ORDER BY lower(u.display_name), u.username, u.id OFFSET $3 LIMIT $4"
    ))
    .persistent(false)
    .bind(workspace.as_uuid())
    .bind(text)
    .bind(offset)
    .bind(limit as i64 + 1)
    .bind(include_deactivated)
    .fetch_all(&mut *conn)
    .await?;
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    Ok(PeoplePage {
        people: rows
            .into_iter()
            .map(person_from_row)
            .collect::<Result<_, _>>()?,
        has_more,
    })
}

/// One person of the workspace, a bot included. A user who is not in the workspace, and a caller
/// who is not, get `not_found`.
pub async fn person(
    pool: &PgPool,
    actor: UserId,
    workspace: WorkspaceId,
    target: UserId,
) -> Result<Person, AppError> {
    let mut conn = pool.acquire().await?;
    require_member(&mut conn, workspace, actor).await?;
    let row: Option<PersonRow> =
        sqlx::query_as(person_select!("WHERE wm.workspace_id = $1 AND u.id = $2"))
            .bind(workspace.as_uuid())
            .bind(target.as_uuid())
            .fetch_optional(&mut *conn)
            .await?;
    row.map(person_from_row)
        .transpose()?
        .ok_or_else(|| AppError::not_found("person not found"))
}

// --- Saved messages --------------------------------------------------------------------------

/// Saves a message for the caller, or removes it from their saved messages.
///
/// The caller must be able to read the message: its conversation is loaded for them as reading the
/// message loads it, and anything else (no such message, a private conversation they are not in)
/// is the message's own `not_found`. A deleted message cannot be saved (`not_found`) but can be
/// removed. Under the conversation's shared lock and the message row `FOR SHARE`, so a removal
/// from the conversation or a deletion of the message cannot interleave. Idempotent: saving what is
/// saved keeps its place and announces nothing; removing what is not saved does nothing. A real
/// change appends `SavedChanged` to the caller alone, with no conversation on the event (it names
/// the message), so a person who only reads a public channel hears of it on their other devices.
pub async fn set_saved(
    pool: &PgPool,
    actor: UserId,
    message: MessageId,
    saved: bool,
) -> Result<SavedResponse, AppError> {
    let conversation: Option<Uuid> =
        sqlx::query_scalar("SELECT conversation_id FROM messages WHERE id = $1")
            .bind(message.as_uuid())
            .fetch_optional(pool)
            .await?;
    let conversation =
        ConversationId::from_uuid(conversation.ok_or_else(messages::message_not_found)?);
    let (mut tx, loaded) = conversations::begin_visible(pool, actor, conversation, Lock::Shared)
        .await
        .map_err(|error| match error {
            AppError::Api(api) if api.code == lazy_protocol::ErrorCode::NotFound => {
                messages::message_not_found()
            }
            other => other,
        })?;
    let deleted: Option<bool> =
        sqlx::query_scalar("SELECT deleted FROM messages WHERE id = $1 FOR SHARE")
            .bind(message.as_uuid())
            .fetch_optional(&mut *tx)
            .await?;
    let deleted = deleted.ok_or_else(messages::message_not_found)?;
    let changed = if saved {
        if deleted {
            return Err(messages::message_not_found());
        }
        // One statement: a second device saving the same message waits for the first and then
        // inserts nothing.
        sqlx::query_scalar::<_, i32>(
            "INSERT INTO saved_messages (user_id, message_id) VALUES ($1, $2) \
             ON CONFLICT (user_id, message_id) DO NOTHING RETURNING 1",
        )
        .bind(actor.as_uuid())
        .bind(message.as_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .is_some()
    } else {
        sqlx::query("DELETE FROM saved_messages WHERE user_id = $1 AND message_id = $2")
            .bind(actor.as_uuid())
            .bind(message.as_uuid())
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0
    };
    if changed {
        events::append_all(
            &mut tx,
            loaded.workspace_id,
            vec![PendingEvent {
                kind: "saved_changed",
                conversation_id: None,
                operation_id: None,
                audience: Audience::User(actor),
                event: Event::SavedChanged {
                    message_id: message,
                    saved,
                },
            }],
        )
        .await?;
    }
    tx.commit().await?;
    Ok(SavedResponse {
        message_id: message,
        saved,
    })
}

type SavedRow = (Uuid, i64, OffsetDateTime, Uuid);

/// The newest saved messages of the person in the workspace that they can read and that are not
/// deleted, below `before`, newest saved first, `limit` of them. The walk follows
/// `saved_messages_list_idx` from the newest, and an item that is deleted or unreadable is skipped
/// inside the limit, so a page holds `limit` eligible items.
macro_rules! saved_select {
    () => {
        "SELECT sm.message_id, sm.saved_order, sm.saved_at, m.conversation_id \
         FROM saved_messages sm \
         JOIN messages m ON m.id = sm.message_id AND NOT m.deleted \
         JOIN conversations c ON c.id = m.conversation_id AND c.workspace_id = $2 \
         WHERE sm.user_id = $1 AND ($3::bigint IS NULL OR sm.saved_order < $3) \
           AND ((c.kind = 'channel' AND c.visibility = 'public') OR EXISTS \
                (SELECT 1 FROM conversation_members cm \
                 WHERE cm.conversation_id = c.id AND cm.user_id = $1)) \
         ORDER BY sm.saved_order DESC LIMIT $4"
    };
}

/// One page of the caller's saved messages in a workspace, most recently saved first, with
/// `before` the cursor of the last item seen. The messages and conversations of the page are loaded
/// with the batch loaders history uses, so a page costs a fixed number of queries.
pub async fn saved(
    pool: &PgPool,
    actor: UserId,
    workspace: WorkspaceId,
    query: SavedQuery,
) -> Result<SavedPage, AppError> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_DIRECTORY_PAGE)
        .clamp(1, MAX_DIRECTORY_PAGE) as usize;
    let before = query
        .before
        .map(|cursor| i64::try_from(cursor).unwrap_or(i64::MAX));
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    require_member(&mut tx, workspace, actor).await?;
    let mut rows: Vec<SavedRow> = sqlx::query_as(saved_select!())
        .persistent(false)
        .bind(actor.as_uuid())
        .bind(workspace.as_uuid())
        .bind(before)
        .bind(limit as i64 + 1)
        .fetch_all(&mut *tx)
        .await?;
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    let ids: Vec<Uuid> = rows.iter().map(|row| row.0).collect();
    let mut found = messages::messages_by_ids(&mut tx, &ids).await?;
    let mut conversation_ids: Vec<Uuid> = rows.iter().map(|row| row.3).collect();
    conversation_ids.sort();
    conversation_ids.dedup();
    let conversations = conversations::conversations_by_ids(&mut tx, &conversation_ids).await?;
    tx.commit().await?;
    let mut items = Vec::with_capacity(rows.len());
    for (message, order, saved_at, conversation) in rows {
        let (Some(found), Some(conversation)) =
            (found.remove(&message), conversations.get(&conversation))
        else {
            continue;
        };
        items.push(SavedItem {
            message: found,
            conversation: conversation.clone(),
            saved_at: Timestamp::from(saved_at),
            cursor: u64::try_from(order)
                .map_err(|_| AppError::internal("stored saved order is negative"))?,
        });
    }
    Ok(SavedPage { items, has_more })
}

/// The ids of the newest [`SAVED_BOOTSTRAP_LIMIT`] saved messages the person can read in the
/// workspace, for the bootstrap, so rows can show the marker.
pub(crate) async fn saved_ids(
    conn: &mut PgConnection,
    user: UserId,
    workspace: WorkspaceId,
) -> Result<Vec<MessageId>, AppError> {
    let rows: Vec<SavedRow> = sqlx::query_as(saved_select!())
        .persistent(false)
        .bind(user.as_uuid())
        .bind(workspace.as_uuid())
        .bind(None::<i64>)
        .bind(SAVED_BOOTSTRAP_LIMIT as i64)
        .fetch_all(conn)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| MessageId::from_uuid(row.0))
        .collect())
}
