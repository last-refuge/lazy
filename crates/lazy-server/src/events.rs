//! The durable per-workspace event log: appending inside transactions, reading for replay, and
//! filtering by what a viewer may currently see.
//!
//! Appending is the heart of the ordering guarantee (llm-docs/DECISIONS.md, D3): [`next_position`]
//! bumps `workspaces.event_head`, and the row lock it takes is held until the caller's transaction
//! ends. Positions are therefore assigned in commit order and never leave a gap.
//!
//! Every event carries an [`Audience`], written when it is appended and used for both replay and
//! live delivery. Audiences are always evaluated against *current* membership, so removing
//! someone from a conversation also withholds everything about it that they had not yet received.

use std::collections::{HashMap, HashSet};

use lazy_domain::{ConversationId, EventSeq, OperationId, Timestamp, UserId, WorkspaceId};
use lazy_protocol::{Event, EventEnvelope};
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::AppError;
use crate::logging::Describe;

/// Postgres `NOTIFY` channel that wakes the dispatcher. The payload is the workspace id; it is
/// only a hint, and the durable `events` table is the source of truth.
pub const NOTIFY_CHANNEL: &str = "lazy_events";

/// Who may receive an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    /// Every member of the workspace.
    Workspace,
    /// The current members of the event's conversation.
    Members,
    /// The current members of the event's conversation, and also this person even if they are not
    /// (or are no longer) a member. Used for removals, which the removed person must learn of.
    MembersAndUser(UserId),
    /// This person only, and only while they are a member of the event's conversation (when it
    /// has one): their own read position and favorites.
    User(UserId),
}

impl Audience {
    fn to_db(self) -> (&'static str, Option<Uuid>) {
        match self {
            Self::Workspace => ("workspace", None),
            Self::Members => ("members", None),
            Self::MembersAndUser(user) => ("members_and_user", Some(user.as_uuid())),
            Self::User(user) => ("user", Some(user.as_uuid())),
        }
    }

    fn from_db(audience: &str, target: Option<Uuid>) -> Result<Self, AppError> {
        let target = target.map(UserId::from_uuid);
        match (audience, target) {
            ("workspace", _) => Ok(Self::Workspace),
            ("members", _) => Ok(Self::Members),
            ("members_and_user", Some(user)) => Ok(Self::MembersAndUser(user)),
            ("user", Some(user)) => Ok(Self::User(user)),
            (other, _) => Err(AppError::internal(format!(
                "unknown event audience {other:?}"
            ))),
        }
    }
}

/// An event as stored, with its position.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEvent {
    pub workspace_id: WorkspaceId,
    pub seq: EventSeq,
    pub occurred_at: Timestamp,
    /// `None` for workspace-wide events.
    pub conversation_id: Option<ConversationId>,
    pub audience: Audience,
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
    pub audience: Audience,
    pub event: &'a Event,
}

/// The message an event is about, if any. Stored with the event so that deleting the message can
/// find and clear the content those events carry.
fn message_of(event: &Event) -> Option<Uuid> {
    match event {
        Event::MessageCreated { message, .. }
        | Event::MessageEdited { message, .. }
        | Event::MessageDeleted { message, .. } => Some(message.id.as_uuid()),
        Event::ReactionsChanged { message_id, .. } => Some(message_id.as_uuid()),
        _ => None,
    }
}

/// Inserts an event at a position obtained from [`next_position`] in the same transaction, and
/// queues a `NOTIFY` that is delivered when that transaction commits.
pub async fn insert_event(conn: &mut PgConnection, new: NewEvent<'_>) -> Result<(), AppError> {
    let payload = serde_json::to_value(new.event)
        .map_err(|e| AppError::internal(format!("cannot serialize event: {e}")))?;
    let (audience, target) = new.audience.to_db();
    sqlx::query(
        "INSERT INTO events \
             (workspace_id, seq, occurred_at, kind, conversation_id, operation_id, audience, target_user_id, message_id, payload) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(new.workspace_id.as_uuid())
    .bind(new.seq.get())
    .bind(new.occurred_at.as_offset_date_time())
    .bind(new.kind)
    .bind(new.conversation_id.map(|id| id.as_uuid()))
    .bind(new.operation_id.map(|id| id.as_uuid()))
    .bind(audience)
    .bind(target)
    .bind(message_of(new.event))
    .bind(sqlx::types::Json(payload))
    .execute(&mut *conn)
    .await?;
    sqlx::query("SELECT pg_notify('lazy_events', $1)")
        .bind(new.workspace_id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

/// An event to append once the rest of a transaction's writes are done.
pub struct PendingEvent {
    pub kind: &'static str,
    pub conversation_id: Option<ConversationId>,
    pub operation_id: Option<OperationId>,
    pub audience: Audience,
    pub event: Event,
}

/// Reserves positions and inserts `events`, in order, returning the positions.
///
/// Call this **last** in a transaction, after every other write. It takes the workspace row lock,
/// which is then held until commit; doing the other work first keeps that lock as short as
/// possible and, more importantly, means a transaction never waits on another lock (a unique
/// index, a conversation row) while holding the workspace lock, which would invite deadlock.
pub async fn append_all(
    conn: &mut PgConnection,
    workspace_id: WorkspaceId,
    events: Vec<PendingEvent>,
) -> Result<Vec<EventSeq>, AppError> {
    if events.len() > 3 {
        return append_batch(conn, workspace_id, events).await;
    }
    let mut positions = Vec::with_capacity(events.len());
    for pending in events {
        let (seq, occurred_at) = next_position(conn, workspace_id).await?;
        insert_event(
            conn,
            NewEvent {
                workspace_id,
                seq,
                occurred_at,
                kind: pending.kind,
                conversation_id: pending.conversation_id,
                operation_id: pending.operation_id,
                audience: pending.audience,
                event: &pending.event,
            },
        )
        .await?;
        positions.push(seq);
    }
    Ok(positions)
}

/// [`append_all`] for many events at once (a person marking hundreds of threads read): one
/// statement reserves all the positions, one inserts all the rows, one notifies. The result is the
/// same rows in the same order as appending them one by one.
async fn append_batch(
    conn: &mut PgConnection,
    workspace_id: WorkspaceId,
    events: Vec<PendingEvent>,
) -> Result<Vec<EventSeq>, AppError> {
    let count = i64::try_from(events.len()).unwrap_or(i64::MAX);
    let (last, now): (i64, OffsetDateTime) = sqlx::query_as(
        "UPDATE workspaces SET event_head = event_head + $2 WHERE id = $1 \
         RETURNING event_head, clock_timestamp()",
    )
    .bind(workspace_id.as_uuid())
    .bind(count)
    .fetch_one(&mut *conn)
    .await?;
    let first = last - count + 1;
    let mut seqs = Vec::with_capacity(events.len());
    let mut kinds: Vec<&str> = Vec::with_capacity(events.len());
    let mut conversations: Vec<Option<Uuid>> = Vec::with_capacity(events.len());
    let mut operations: Vec<Option<Uuid>> = Vec::with_capacity(events.len());
    let mut audiences: Vec<&str> = Vec::with_capacity(events.len());
    let mut targets: Vec<Option<Uuid>> = Vec::with_capacity(events.len());
    let mut messages: Vec<Option<Uuid>> = Vec::with_capacity(events.len());
    let mut payloads: Vec<serde_json::Value> = Vec::with_capacity(events.len());
    for (index, pending) in events.iter().enumerate() {
        let (audience, target) = pending.audience.to_db();
        seqs.push(first + index as i64);
        kinds.push(pending.kind);
        conversations.push(pending.conversation_id.map(|id| id.as_uuid()));
        operations.push(pending.operation_id.map(|id| id.as_uuid()));
        audiences.push(audience);
        targets.push(target);
        messages.push(message_of(&pending.event));
        payloads.push(
            serde_json::to_value(&pending.event)
                .map_err(|e| AppError::internal(format!("cannot serialize event: {e}")))?,
        );
    }
    sqlx::query(
        "INSERT INTO events \
             (workspace_id, seq, occurred_at, kind, conversation_id, operation_id, audience, target_user_id, message_id, payload) \
         SELECT $1, e.seq, $2, e.kind, e.conversation_id, e.operation_id, e.audience, e.target_user_id, e.message_id, e.payload \
         FROM unnest($3::bigint[], $4::text[], $5::uuid[], $6::uuid[], $7::text[], $8::uuid[], $9::uuid[], $10::jsonb[]) \
              AS e(seq, kind, conversation_id, operation_id, audience, target_user_id, message_id, payload)",
    )
    .bind(workspace_id.as_uuid())
    .bind(now)
    .bind(&seqs)
    .bind(&kinds)
    .bind(&conversations)
    .bind(&operations)
    .bind(&audiences)
    .bind(&targets)
    .bind(&messages)
    .bind(&payloads)
    .execute(&mut *conn)
    .await?;
    sqlx::query("SELECT pg_notify('lazy_events', $1)")
        .bind(workspace_id.to_string())
        .execute(conn)
        .await?;
    Ok(seqs.into_iter().filter_map(EventSeq::new).collect())
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
        Option<String>,
        Option<Uuid>,
        Option<sqlx::types::Json<serde_json::Value>>,
    );
    // One statement, hence one snapshot. The lateral subquery lets the primary key's index and the
    // LIMIT bound the work to one page however large the backlog is (a plain outer join makes
    // Postgres scan and sort every later event). The outer join always yields the workspace row, so
    // the floor is known even when no events follow the cursor; that row has a NULL `seq`.
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT w.event_floor, e.seq, e.occurred_at, e.conversation_id, e.audience, e.target_user_id, e.payload \
         FROM workspaces w \
         LEFT JOIN LATERAL ( \
             SELECT seq, occurred_at, conversation_id, audience, target_user_id, payload FROM events \
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
    for (_, seq, occurred_at, conversation_id, audience, target, payload) in rows {
        let (Some(seq), Some(occurred_at), Some(audience), Some(payload)) =
            (seq, occurred_at, audience, payload)
        else {
            continue;
        };
        let audience = Audience::from_db(&audience, target)?;
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
                audience,
                event,
            }),
            Err(error) => {
                tracing::error!(%workspace_id, seq = seq.get(), error = %error.describe(), "skipping undecodable event");
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

/// Evaluates each event's stored audience against the viewer's **current** membership.
///
/// Membership is read from the database once per distinct conversation in the batch, not once per
/// event, after the batch was read. So a person removed from a conversation stops receiving
/// everything about it at once, including events that committed before the removal but have not
/// been delivered, and a person added later receives the conversation's older events when they
/// replay past them.
pub fn is_visible(viewer: UserId, event: &StoredEvent, member_of: &HashMap<Uuid, bool>) -> bool {
    let is_member = |conversation: Option<ConversationId>| {
        conversation
            .and_then(|id| member_of.get(&id.as_uuid()).copied())
            .unwrap_or(false)
    };
    match event.audience {
        Audience::Workspace => true,
        Audience::Members => is_member(event.conversation_id),
        Audience::MembersAndUser(user) => viewer == user || is_member(event.conversation_id),
        Audience::User(user) => {
            viewer == user && (event.conversation_id.is_none() || is_member(event.conversation_id))
        }
    }
}

/// See [`is_visible`].
pub async fn visibility(
    pool: &PgPool,
    viewer: UserId,
    workspace_id: WorkspaceId,
    events: &[&StoredEvent],
) -> Result<Visibility, AppError> {
    // A deactivated membership is no membership: the subscription ends at the next delivery.
    let in_workspace =
        crate::access::active_role(&mut *pool.acquire().await?, workspace_id, viewer)
            .await?
            .is_some();
    if !in_workspace {
        return Ok(Visibility::NotAMember);
    }

    // Only events whose audience depends on membership need the lookup.
    let conversation_ids: Vec<Uuid> = events
        .iter()
        .filter(|event| !matches!(event.audience, Audience::Workspace))
        .filter_map(|event| event.conversation_id)
        .map(|id| id.as_uuid())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let mut member_of: HashMap<Uuid, bool> = HashMap::with_capacity(conversation_ids.len());
    if !conversation_ids.is_empty() {
        let rows: Vec<(Uuid, bool)> = sqlx::query_as(
            "SELECT c.id, (cm.user_id IS NOT NULL) \
             FROM conversations c \
             LEFT JOIN conversation_members cm ON cm.conversation_id = c.id AND cm.user_id = $1 \
             WHERE c.workspace_id = $2 AND c.id = ANY($3)",
        )
        .bind(viewer.as_uuid())
        .bind(workspace_id.as_uuid())
        .bind(&conversation_ids)
        .fetch_all(pool)
        .await?;
        member_of.extend(rows);
    }

    Ok(Visibility::Flags(
        events
            .iter()
            .map(|event| is_visible(viewer, event, &member_of))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use lazy_protocol::{Conversation, ConversationUserState};

    use super::*;

    fn stored(audience: Audience, conversation: Option<ConversationId>) -> StoredEvent {
        // The payload is irrelevant to audience rules; any event will do.
        let state = ConversationUserState {
            conversation_id: conversation.unwrap_or_else(ConversationId::generate),
            favorite: false,
            last_read: EventSeq::ZERO,
            unread_count: 0,
            mention_count: 0,
        };
        let _: Option<Conversation> = None;
        StoredEvent {
            workspace_id: WorkspaceId::generate(),
            seq: EventSeq::new(1).unwrap(),
            occurred_at: Timestamp::now(),
            conversation_id: conversation,
            audience,
            event: Event::UserStateChanged { state },
        }
    }

    fn membership(entries: &[(ConversationId, bool)]) -> HashMap<Uuid, bool> {
        entries
            .iter()
            .map(|(id, member)| (id.as_uuid(), *member))
            .collect()
    }

    #[test]
    fn audiences_are_evaluated_against_current_membership() {
        let (me, other) = (UserId::generate(), UserId::generate());
        let (joined, not_joined) = (ConversationId::generate(), ConversationId::generate());
        let membership = membership(&[(joined, true), (not_joined, false)]);

        // Workspace events reach everyone.
        assert!(is_visible(
            me,
            &stored(Audience::Workspace, None),
            &membership
        ));

        // Members-only events need membership now.
        assert!(is_visible(
            me,
            &stored(Audience::Members, Some(joined)),
            &membership
        ));
        assert!(!is_visible(
            me,
            &stored(Audience::Members, Some(not_joined)),
            &membership
        ));
        assert!(
            !is_visible(
                me,
                &stored(Audience::Members, Some(ConversationId::generate())),
                &membership
            ),
            "an unknown conversation is never visible"
        );

        // A removal reaches its subject even without membership, and nobody else outside.
        let removal = stored(Audience::MembersAndUser(me), Some(not_joined));
        assert!(is_visible(me, &removal, &membership));
        assert!(!is_visible(other, &removal, &membership));

        // Personal state reaches its owner, and only while they are in the conversation.
        let mine = stored(Audience::User(me), Some(joined));
        assert!(is_visible(me, &mine, &membership));
        assert!(!is_visible(other, &mine, &membership));
        assert!(!is_visible(
            me,
            &stored(Audience::User(me), Some(not_joined)),
            &membership
        ));
        assert!(is_visible(
            me,
            &stored(Audience::User(me), None),
            &membership
        ));
    }

    #[test]
    fn audiences_roundtrip_through_their_stored_form() {
        let user = UserId::generate();
        for audience in [
            Audience::Workspace,
            Audience::Members,
            Audience::MembersAndUser(user),
            Audience::User(user),
        ] {
            let (kind, target) = audience.to_db();
            assert_eq!(Audience::from_db(kind, target).unwrap(), audience);
        }
        assert!(Audience::from_db("user", None).is_err());
        assert!(Audience::from_db("everyone", None).is_err());
    }
}
