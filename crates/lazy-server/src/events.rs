//! The durable per-workspace event log: appending inside transactions, reading for replay, and
//! filtering by what a viewer may currently see.
//!
//! Appending is the heart of the ordering guarantee (llm-docs/DECISIONS.md, D3): [`next_position`]
//! bumps `workspaces.event_head`, and the row lock it takes is held until the caller's transaction
//! ends. Positions are therefore assigned in commit order and never leave a gap.

use std::collections::{HashMap, HashSet};

use lazy_domain::{
    ChannelAccess, ConversationId, EventSeq, OperationId, Timestamp, UserId, WorkspaceId,
};
use lazy_protocol::{Event, EventEnvelope};
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::db;
use crate::error::AppError;

/// Postgres `NOTIFY` channel that wakes the dispatcher. The payload is the workspace id; it is
/// only a hint, and the durable `events` table is the source of truth.
pub const NOTIFY_CHANNEL: &str = "lazy_events";

/// An event as stored, with its position.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEvent {
    pub workspace_id: WorkspaceId,
    pub seq: EventSeq,
    pub occurred_at: Timestamp,
    /// `None` for workspace-wide events.
    pub conversation_id: Option<ConversationId>,
    pub event: Event,
}

impl StoredEvent {
    pub fn envelope(&self) -> EventEnvelope {
        EventEnvelope {
            workspace_id: self.workspace_id,
            seq: self.seq,
            occurred_at: self.occurred_at,
            event: self.event.clone(),
        }
    }
}

/// Reserves the next position in a workspace's log and returns it with the database clock.
///
/// Must be called inside the transaction that will insert the event, and as late as is
/// practical: the row lock it takes serializes event-producing writes within the workspace until
/// that transaction commits or rolls back.
pub async fn next_position(
    conn: &mut PgConnection,
    workspace_id: WorkspaceId,
) -> Result<(EventSeq, Timestamp), AppError> {
    let (head, now): (i64, OffsetDateTime) = sqlx::query_as(
        "UPDATE workspaces SET event_head = event_head + 1 WHERE id = $1 \
         RETURNING event_head, clock_timestamp()",
    )
    .bind(workspace_id.as_uuid())
    .fetch_one(conn)
    .await?;
    let seq = EventSeq::new(head)
        .ok_or_else(|| AppError::internal("workspace event head is negative"))?;
    Ok((seq, Timestamp::from(now)))
}

/// An event about to be appended to a workspace's log.
pub struct NewEvent<'a> {
    pub workspace_id: WorkspaceId,
    /// A position obtained from [`next_position`] in the same transaction.
    pub seq: EventSeq,
    pub occurred_at: Timestamp,
    /// Short machine-readable label stored beside the payload, such as `message_created`.
    pub kind: &'static str,
    /// `None` for workspace-wide events.
    pub conversation_id: Option<ConversationId>,
    pub operation_id: Option<OperationId>,
    pub event: &'a Event,
}

/// Inserts an event at a position obtained from [`next_position`] in the same transaction, and
/// queues a `NOTIFY` that is delivered when that transaction commits.
pub async fn insert_event(conn: &mut PgConnection, new: NewEvent<'_>) -> Result<(), AppError> {
    let payload = serde_json::to_value(new.event)
        .map_err(|e| AppError::internal(format!("cannot serialize event: {e}")))?;
    sqlx::query(
        "INSERT INTO events (workspace_id, seq, occurred_at, kind, conversation_id, operation_id, payload) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(new.workspace_id.as_uuid())
    .bind(new.seq.get())
    .bind(new.occurred_at.as_offset_date_time())
    .bind(new.kind)
    .bind(new.conversation_id.map(|id| id.as_uuid()))
    .bind(new.operation_id.map(|id| id.as_uuid()))
    .bind(sqlx::types::Json(payload))
    .execute(&mut *conn)
    .await?;
    sqlx::query("SELECT pg_notify('lazy_events', $1)")
        .bind(new.workspace_id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

/// The newest committed position in a workspace's log, or `None` if the workspace is unknown.
pub async fn head(pool: &PgPool, workspace_id: WorkspaceId) -> Result<Option<EventSeq>, AppError> {
    let head: Option<i64> = sqlx::query_scalar("SELECT event_head FROM workspaces WHERE id = $1")
        .bind(workspace_id.as_uuid())
        .fetch_optional(pool)
        .await?;
    head.map(|value| {
        EventSeq::new(value).ok_or_else(|| AppError::internal("workspace event head is negative"))
    })
    .transpose()
}

/// One read of the log.
#[derive(Debug)]
pub struct EventPage {
    /// Events that could be decoded, in position order.
    pub events: Vec<StoredEvent>,
    /// How many log rows the read returned, including any that could not be decoded.
    pub scanned: usize,
    /// Position of the last row returned, or the `after` argument if there were none.
    pub scanned_to: EventSeq,
    /// The workspace's `event_floor` in the same snapshot as `events`: events at or below it have
    /// been pruned. When `floor > after` the read did **not** cover `(after, floor]`, and the
    /// caller must not treat `events` as contiguous with `after`.
    pub floor: EventSeq,
}

/// Reads up to `limit` committed events with position greater than `after`, oldest first.
///
/// The events and the workspace's retention floor come from one SQL statement, hence one
/// snapshot. A prune deletes events and raises the floor in a single transaction, so a read can
/// never see the events gone without also seeing the raised floor. That is what lets callers
/// detect, rather than silently skip, events removed since their cursor.
///
/// A row whose payload cannot be decoded is logged and skipped, but still counts toward
/// `scanned` and `scanned_to` so the stream keeps moving.
pub async fn fetch_after(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    after: EventSeq,
    limit: usize,
) -> Result<EventPage, AppError> {
    type Row = (
        i64,
        Option<i64>,
        Option<OffsetDateTime>,
        Option<Uuid>,
        Option<sqlx::types::Json<serde_json::Value>>,
    );
    // One statement, hence one snapshot. The lateral subquery lets the primary key's index and the
    // LIMIT bound the work to one page however large the backlog is (a plain outer join makes
    // Postgres scan and sort every later event). The outer join always yields the workspace row, so
    // the floor is known even when no events follow the cursor; that row has a NULL `seq`.
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT w.event_floor, e.seq, e.occurred_at, e.conversation_id, e.payload \
         FROM workspaces w \
         LEFT JOIN LATERAL ( \
             SELECT seq, occurred_at, conversation_id, payload FROM events \
             WHERE workspace_id = w.id AND seq > $2 ORDER BY seq LIMIT $3 \
         ) e ON true \
         WHERE w.id = $1 ORDER BY e.seq",
    )
    .bind(workspace_id.as_uuid())
    .bind(after.get())
    .bind(i64::try_from(limit).unwrap_or(i64::MAX))
    .fetch_all(pool)
    .await?;

    let Some(first) = rows.first() else {
        return Err(AppError::not_found("workspace not found"));
    };
    let floor = EventSeq::new(first.0)
        .ok_or_else(|| AppError::internal("workspace event floor is negative"))?;

    let mut scanned = 0;
    let mut scanned_to = after;
    let mut events = Vec::with_capacity(rows.len());
    for (_, seq, occurred_at, conversation_id, payload) in rows {
        let (Some(seq), Some(occurred_at), Some(payload)) = (seq, occurred_at, payload) else {
            continue;
        };
        let seq =
            EventSeq::new(seq).ok_or_else(|| AppError::internal("negative event position"))?;
        scanned += 1;
        scanned_to = seq;
        match serde_json::from_value::<Event>(payload.0) {
            Ok(event) => events.push(StoredEvent {
                workspace_id,
                seq,
                occurred_at: Timestamp::from(occurred_at),
                conversation_id: conversation_id.map(ConversationId::from_uuid),
                event,
            }),
            Err(error) => {
                tracing::error!(%workspace_id, seq = seq.get(), %error, "skipping undecodable event");
            }
        }
    }
    Ok(EventPage {
        events,
        scanned,
        scanned_to,
        floor,
    })
}

/// Which events of a batch a viewer may see right now.
#[derive(Debug, PartialEq, Eq)]
pub enum Visibility {
    /// The viewer is not (or no longer) a member of the workspace and may see nothing.
    NotAMember,
    /// One flag per input event, in order.
    Flags(Vec<bool>),
}

/// Evaluates the viewer's current access against the database. Conversation access is checked
/// once per distinct conversation in the batch, not once per event.
pub async fn visibility(
    pool: &PgPool,
    viewer: UserId,
    workspace_id: WorkspaceId,
    events: &[&StoredEvent],
) -> Result<Visibility, AppError> {
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(workspace_id.as_uuid())
    .bind(viewer.as_uuid())
    .fetch_optional(pool)
    .await?;
    let Some(role) = role else {
        return Ok(Visibility::NotAMember);
    };
    let role = db::role_from_db(&role)?;

    let conversation_ids: Vec<Uuid> = events
        .iter()
        .filter_map(|event| event.conversation_id)
        .map(|id| id.as_uuid())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let mut readable: HashMap<Uuid, bool> = HashMap::with_capacity(conversation_ids.len());
    if !conversation_ids.is_empty() {
        let rows: Vec<(Uuid, String, bool, bool)> = sqlx::query_as(
            "SELECT c.id, c.visibility, c.archived, (cm.user_id IS NOT NULL) \
             FROM conversations c \
             LEFT JOIN conversation_members cm ON cm.conversation_id = c.id AND cm.user_id = $1 \
             WHERE c.workspace_id = $2 AND c.id = ANY($3)",
        )
        .bind(viewer.as_uuid())
        .bind(workspace_id.as_uuid())
        .bind(&conversation_ids)
        .fetch_all(pool)
        .await?;
        for (id, visibility, archived, is_member) in rows {
            let access = ChannelAccess {
                workspace_role: Some(role),
                is_channel_member: is_member,
                visibility: db::visibility_from_db(&visibility)?,
                archived,
            };
            readable.insert(id, access.can_read());
        }
    }

    let flags = events
        .iter()
        .map(|event| match event.conversation_id {
            None => true,
            Some(id) => readable.get(&id.as_uuid()).copied().unwrap_or(false),
        })
        .collect();
    Ok(Visibility::Flags(flags))
}
