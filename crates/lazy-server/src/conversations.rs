//! Conversations: creating channels, opening direct and group conversations, browsing, managing
//! membership, favorites, and read state.
//!
//! # How every command is shaped
//!
//! 1. **Lock the conversation row** (`FOR NO KEY UPDATE` to change it, `FOR SHARE` to act inside
//!    it, as sends do). Concurrent commands on one conversation therefore serialize, and a send
//!    can never interleave with an archive or a removal.
//! 2. **Load the caller's facts** and evaluate [`ConversationFacts`]. A conversation the caller
//!    cannot see (a private channel they are not in, or any conversation outside their workspace)
//!    is `not_found`; one they can see but may not do this to is `forbidden`.
//! 3. **Write** rows: memberships, state, the conversation itself, the member count.
//! 4. **Append events last** with [`events::append_all`], which takes the workspace row lock for
//!    the position bump. Every transaction acquires locks in the order *conversation, then
//!    workspace*, and waits on nothing else once it holds the workspace lock, so none of them
//!    can deadlock with another.
//!
//! The commands that carry an operation id (create channel, open conversation) additionally claim
//! a [receipt](crate::receipts) so a repeat returns the original result.

use std::collections::HashMap;

use lazy_domain::conversation::{description_is_valid, topic_is_valid};
use lazy_domain::{
    ChannelAccess, ConversationFacts, ConversationId, ConversationKind, EventSeq, OperationId,
    ParticipantSet, UserId, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    AddMembersRequest, BrowseChannelsResponse, BrowsedChannel, Conversation, ConversationUserState,
    CreateChannelRequest, ErrorCode, Event, OpenConversationRequest, UpdateConversationRequest,
};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::accounts;
use crate::crypto;
use crate::db;
use crate::error::AppError;
use crate::events::{self, Audience, PendingEvent};
use crate::receipts::{self, ReceiptKind, ReceiptTarget};

/// Most people one `add members` request may name.
const MAX_ADD_USERS: usize = 100;
/// Most channels the directory returns.
const MAX_BROWSE: i64 = 1000;

/// The columns of a conversation, selected under the alias `c`.
macro_rules! conversation_columns {
    () => {
        "c.id, c.workspace_id, c.kind, c.name, c.topic, c.description, c.visibility, c.archived, \
         c.is_default, c.created_by, c.member_count"
    };
}

#[derive(sqlx::FromRow)]
struct ConversationRow {
    id: Uuid,
    workspace_id: Uuid,
    kind: String,
    name: Option<String>,
    topic: Option<String>,
    description: Option<String>,
    visibility: String,
    archived: bool,
    is_default: bool,
    created_by: Option<Uuid>,
    member_count: i32,
}

impl ConversationRow {
    fn into_conversation(self, participants: Vec<UserId>) -> Result<Conversation, AppError> {
        Ok(Conversation {
            id: ConversationId::from_uuid(self.id),
            workspace_id: WorkspaceId::from_uuid(self.workspace_id),
            kind: db::kind_from_db(&self.kind)?,
            name: self
                .name
                .as_deref()
                .map(db::channel_name_from_db)
                .transpose()?,
            topic: self.topic,
            description: self.description,
            visibility: db::visibility_from_db(&self.visibility)?,
            archived: self.archived,
            is_default: self.is_default,
            created_by: self.created_by.map(UserId::from_uuid),
            participants,
            member_count: u32::try_from(self.member_count).unwrap_or(0),
        })
    }
}

#[derive(sqlx::FromRow)]
struct LoadedRow {
    #[sqlx(flatten)]
    conversation: ConversationRow,
    role: Option<String>,
    is_member: bool,
}

/// A conversation together with one person's relationship to it.
pub struct Loaded {
    pub id: ConversationId,
    pub workspace_id: WorkspaceId,
    pub facts: ConversationFacts,
    row: ConversationRow,
}

impl Loaded {
    /// Whether the person may read it: a workspace member, and either in it or it is public.
    pub fn can_read(&self) -> bool {
        self.access().can_read()
    }

    /// The simple read/post rules used by messages.
    pub fn access(&self) -> ChannelAccess {
        ChannelAccess {
            workspace_role: self.facts.workspace_role,
            is_channel_member: self.facts.is_member,
            visibility: self.facts.visibility,
            archived: self.facts.archived,
        }
    }
}

/// Loads a conversation and `actor`'s relationship to it, or `None` if it does not exist.
pub async fn load(
    conn: &mut PgConnection,
    actor: UserId,
    id: ConversationId,
) -> Result<Option<Loaded>, AppError> {
    let row: Option<LoadedRow> = sqlx::query_as(concat!(
        "SELECT ",
        conversation_columns!(),
        ", wm.role, (cm.user_id IS NOT NULL) AS is_member \
         FROM conversations c \
         LEFT JOIN workspace_members wm ON wm.workspace_id = c.workspace_id AND wm.user_id = $1 \
         LEFT JOIN conversation_members cm ON cm.conversation_id = c.id AND cm.user_id = $1 \
         WHERE c.id = $2"
    ))
    .bind(actor.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(conn)
    .await?;
    row.map(|loaded| {
        let conversation = &loaded.conversation;
        let facts = ConversationFacts {
            workspace_role: loaded.role.as_deref().map(db::role_from_db).transpose()?,
            is_member: loaded.is_member,
            is_creator: conversation.created_by == Some(actor.as_uuid()),
            kind: db::kind_from_db(&conversation.kind)?,
            visibility: db::visibility_from_db(&conversation.visibility)?,
            archived: conversation.archived,
            is_default: conversation.is_default,
        };
        Ok(Loaded {
            id,
            workspace_id: WorkspaceId::from_uuid(conversation.workspace_id),
            facts,
            row: loaded.conversation,
        })
    })
    .transpose()
}

/// How a transaction holds a conversation's row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lock {
    /// Acting inside the conversation (sending, favoriting, reading): many at once, but never
    /// concurrently with a change to the conversation itself.
    Shared,
    /// Changing the conversation: name, archive state, membership, member count.
    Exclusive,
}

async fn lock(conn: &mut PgConnection, id: ConversationId, mode: Lock) -> Result<bool, AppError> {
    let sql = match mode {
        Lock::Shared => "SELECT true FROM conversations WHERE id = $1 FOR SHARE",
        Lock::Exclusive => "SELECT true FROM conversations WHERE id = $1 FOR NO KEY UPDATE",
    };
    let found: Option<bool> = sqlx::query_scalar(sql)
        .bind(id.as_uuid())
        .fetch_optional(conn)
        .await?;
    Ok(found.is_some())
}

/// Starts a transaction holding the conversation's row and returns it with the caller's facts.
/// A conversation that does not exist or that the caller cannot see is `not_found`.
pub async fn begin_visible(
    pool: &PgPool,
    actor: UserId,
    id: ConversationId,
    mode: Lock,
) -> Result<(Transaction<'static, Postgres>, Loaded), AppError> {
    let mut tx = pool.begin().await?;
    if !lock(&mut tx, id, mode).await? {
        return Err(not_found());
    }
    // A fresh statement after the lock sees everything committed before the lock was granted.
    let loaded = load(&mut tx, actor, id)
        .await?
        .filter(Loaded::can_read)
        .ok_or_else(not_found)?;
    Ok((tx, loaded))
}

fn not_found() -> AppError {
    AppError::not_found("conversation not found")
}

fn forbidden(action: &str) -> AppError {
    AppError::forbidden(format!("you cannot {action}"))
}

/// A person in the workspace, or `not_found`.
async fn require_workspace_member(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    user: UserId,
) -> Result<WorkspaceRole, AppError> {
    accounts::workspace_role(pool, workspace_id, user)
        .await?
        .ok_or_else(|| AppError::not_found("workspace not found"))
}

// --- Reading conversations ------------------------------------------------------------------

/// The people in a direct or group conversation, in the order [`ParticipantSet`] uses.
async fn participants_of(
    conn: &mut PgConnection,
    id: ConversationId,
) -> Result<Vec<UserId>, AppError> {
    let ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT user_id FROM conversation_members WHERE conversation_id = $1")
            .bind(id.as_uuid())
            .fetch_all(conn)
            .await?;
    let mut users: Vec<UserId> = ids.into_iter().map(UserId::from_uuid).collect();
    users.sort();
    Ok(users)
}

/// The conversation as everyone sees it (nothing depends on who is asking).
pub async fn conversation_dto(
    conn: &mut PgConnection,
    id: ConversationId,
) -> Result<Conversation, AppError> {
    let row: ConversationRow = sqlx::query_as(concat!(
        "SELECT ",
        conversation_columns!(),
        " FROM conversations c WHERE c.id = $1"
    ))
    .bind(id.as_uuid())
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(not_found)?;
    let participants = if db::kind_from_db(&row.kind)? == ConversationKind::Channel {
        Vec::new()
    } else {
        participants_of(conn, id).await?
    };
    row.into_conversation(participants)
}

/// Conversations (by id) of one workspace that `user` is a member of, with their state. Used by
/// bootstrap; the caller supplies the connection so it reads from its snapshot.
pub async fn member_conversations(
    conn: &mut PgConnection,
    user: UserId,
    workspace_id: WorkspaceId,
) -> Result<(Vec<Conversation>, Vec<ConversationUserState>), AppError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        #[sqlx(flatten)]
        conversation: ConversationRow,
        favorite: Option<bool>,
        last_read: Option<i64>,
    }
    let rows: Vec<Row> = sqlx::query_as(concat!(
        "SELECT ",
        conversation_columns!(),
        ", us.favorite, us.last_read \
         FROM conversation_members cm \
         JOIN conversations c ON c.id = cm.conversation_id \
         LEFT JOIN conversation_user_state us ON us.user_id = cm.user_id AND us.conversation_id = c.id \
         WHERE cm.user_id = $1 AND c.workspace_id = $2 \
         ORDER BY c.name NULLS LAST, c.created_at, c.id"
    ))
    .bind(user.as_uuid())
    .bind(workspace_id.as_uuid())
    .fetch_all(&mut *conn)
    .await?;

    // Participants of direct and group conversations, in one query.
    let people_ids: Vec<Uuid> = rows
        .iter()
        .filter(|row| row.conversation.kind != "channel")
        .map(|row| row.conversation.id)
        .collect();
    let mut participants: HashMap<Uuid, Vec<UserId>> = HashMap::new();
    if !people_ids.is_empty() {
        let pairs: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT conversation_id, user_id FROM conversation_members WHERE conversation_id = ANY($1)",
        )
        .bind(&people_ids)
        .fetch_all(&mut *conn)
        .await?;
        for (conversation, member) in pairs {
            participants
                .entry(conversation)
                .or_default()
                .push(UserId::from_uuid(member));
        }
        for members in participants.values_mut() {
            members.sort();
        }
    }

    let mut conversations = Vec::with_capacity(rows.len());
    let mut states = Vec::with_capacity(rows.len());
    for row in rows {
        let id = ConversationId::from_uuid(row.conversation.id);
        let last_read = row.last_read.unwrap_or(0);
        states.push(ConversationUserState {
            conversation_id: id,
            favorite: row.favorite.unwrap_or(false),
            last_read: seq_from(last_read)?,
            // One query per conversation, each with its own read position as a concrete value, so
            // the planner picks an index range scan. A single correlated subquery would have to
            // guess the position and chooses a scan of the whole table instead.
            unread_count: unread_count(conn, user, id, last_read).await?,
            mention_count: 0,
        });
        let people = participants
            .remove(&row.conversation.id)
            .unwrap_or_default();
        conversations.push(row.conversation.into_conversation(people)?);
    }
    Ok((conversations, states))
}

fn seq_from(value: i64) -> Result<EventSeq, AppError> {
    EventSeq::new(value).ok_or_else(|| AppError::internal("stored position is negative"))
}

/// Messages from other people after `last_read`.
async fn unread_count(
    conn: &mut PgConnection,
    user: UserId,
    conversation: ConversationId,
    last_read: i64,
) -> Result<u32, AppError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM messages WHERE conversation_id = $1 AND seq > $2 AND author_id <> $3",
    )
    .bind(conversation.as_uuid())
    .bind(last_read)
    .bind(user.as_uuid())
    .fetch_one(conn)
    .await?;
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

/// `user`'s state for `conversation`, or `None` if they have none (they are not a member).
async fn user_state(
    conn: &mut PgConnection,
    user: UserId,
    conversation: ConversationId,
) -> Result<Option<ConversationUserState>, AppError> {
    let row: Option<(bool, i64)> = sqlx::query_as(
        "SELECT favorite, last_read FROM conversation_user_state WHERE user_id = $1 AND conversation_id = $2",
    )
    .bind(user.as_uuid())
    .bind(conversation.as_uuid())
    .fetch_optional(&mut *conn)
    .await?;
    let Some((favorite, last_read)) = row else {
        return Ok(None);
    };
    Ok(Some(ConversationUserState {
        conversation_id: conversation,
        favorite,
        last_read: seq_from(last_read)?,
        unread_count: unread_count(conn, user, conversation, last_read).await?,
        mention_count: 0,
    }))
}

/// The public channels of a workspace, ordered by name, with whether the caller has joined each.
pub async fn browse_channels(
    pool: &PgPool,
    actor: UserId,
    workspace_id: WorkspaceId,
) -> Result<BrowseChannelsResponse, AppError> {
    require_workspace_member(pool, workspace_id, actor).await?;
    #[derive(sqlx::FromRow)]
    struct Row {
        #[sqlx(flatten)]
        conversation: ConversationRow,
        joined: bool,
    }
    let rows: Vec<Row> = sqlx::query_as(concat!(
        "SELECT ",
        conversation_columns!(),
        ", (cm.user_id IS NOT NULL) AS joined \
         FROM conversations c \
         LEFT JOIN conversation_members cm ON cm.conversation_id = c.id AND cm.user_id = $1 \
         WHERE c.workspace_id = $2 AND c.kind = 'channel' AND c.visibility = 'public' \
         ORDER BY c.name LIMIT $3"
    ))
    .bind(actor.as_uuid())
    .bind(workspace_id.as_uuid())
    .bind(MAX_BROWSE)
    .fetch_all(pool)
    .await?;
    let channels = rows
        .into_iter()
        .map(|row| {
            Ok(BrowsedChannel {
                conversation: row.conversation.into_conversation(Vec::new())?,
                joined: row.joined,
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    Ok(BrowseChannelsResponse { channels })
}

// --- Membership primitives ------------------------------------------------------------------

/// Adds `users` to a conversation, skipping people already in it, and returns those newly added
/// in the order given. Each new member's read position starts at the conversation's newest
/// message, so history from before they joined does not count as unread. The member count is
/// updated in the same statement sequence, under the caller's lock on the conversation row.
pub async fn add_members(
    conn: &mut PgConnection,
    conversation: ConversationId,
    users: &[UserId],
) -> Result<Vec<UserId>, AppError> {
    if users.is_empty() {
        return Ok(Vec::new());
    }
    let wanted: Vec<Uuid> = users.iter().map(|user| user.as_uuid()).collect();
    let inserted: Vec<Uuid> = sqlx::query_scalar(
        "INSERT INTO conversation_members (conversation_id, user_id) \
         SELECT $1, u FROM unnest($2::uuid[]) AS u ON CONFLICT DO NOTHING RETURNING user_id",
    )
    .bind(conversation.as_uuid())
    .bind(&wanted)
    .fetch_all(&mut *conn)
    .await?;
    if inserted.is_empty() {
        return Ok(Vec::new());
    }

    sqlx::query(
        "INSERT INTO conversation_user_state (user_id, conversation_id, last_read) \
         SELECT u, $1, COALESCE((SELECT max(seq) FROM messages WHERE conversation_id = $1), 0) \
         FROM unnest($2::uuid[]) AS u \
         ON CONFLICT (user_id, conversation_id) DO UPDATE \
         SET last_read = GREATEST(conversation_user_state.last_read, excluded.last_read)",
    )
    .bind(conversation.as_uuid())
    .bind(&inserted)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE conversations SET member_count = member_count + $2 WHERE id = $1")
        .bind(conversation.as_uuid())
        .bind(i32::try_from(inserted.len()).unwrap_or(i32::MAX))
        .execute(conn)
        .await?;

    Ok(users
        .iter()
        .copied()
        .filter(|user| inserted.contains(&user.as_uuid()))
        .collect())
}

/// Removes someone from a conversation along with their state there. Returns whether they were a
/// member. The caller holds the conversation's exclusive lock.
async fn remove_member(
    conn: &mut PgConnection,
    conversation: ConversationId,
    user: UserId,
) -> Result<bool, AppError> {
    let removed =
        sqlx::query("DELETE FROM conversation_members WHERE conversation_id = $1 AND user_id = $2")
            .bind(conversation.as_uuid())
            .bind(user.as_uuid())
            .execute(&mut *conn)
            .await?
            .rows_affected()
            == 1;
    if removed {
        sqlx::query(
            "DELETE FROM conversation_user_state WHERE conversation_id = $1 AND user_id = $2",
        )
        .bind(conversation.as_uuid())
        .bind(user.as_uuid())
        .execute(&mut *conn)
        .await?;
        sqlx::query("UPDATE conversations SET member_count = member_count - 1 WHERE id = $1")
            .bind(conversation.as_uuid())
            .execute(conn)
            .await?;
    }
    Ok(removed)
}

/// Joins the workspace's default channel, if it has one, for a person who just registered.
/// Returns the channel as it is after they joined. Caller is inside the registration transaction.
pub async fn join_default_channel(
    conn: &mut PgConnection,
    workspace_id: WorkspaceId,
    user: UserId,
) -> Result<Option<Conversation>, AppError> {
    let default: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM conversations WHERE workspace_id = $1 AND is_default FOR NO KEY UPDATE",
    )
    .bind(workspace_id.as_uuid())
    .fetch_optional(&mut *conn)
    .await?;
    let Some(id) = default.map(ConversationId::from_uuid) else {
        return Ok(None);
    };
    add_members(conn, id, &[user]).await?;
    Ok(Some(conversation_dto(conn, id).await?))
}

// --- Commands --------------------------------------------------------------------------------

/// Treats an empty string as "no text" and checks the rest with `valid`.
fn optional_text(
    value: Option<&str>,
    valid: fn(&str) -> bool,
    what: &str,
) -> Result<Option<String>, AppError> {
    match value {
        None | Some("") => Ok(None),
        Some(text) if valid(text) => Ok(Some(text.to_owned())),
        Some(_) => Err(AppError::invalid(format!("that {what} is not allowed"))),
    }
}

/// Creates a channel with the caller as its first member and its creator.
pub async fn create_channel(
    pool: &PgPool,
    actor: UserId,
    workspace_id: WorkspaceId,
    request: CreateChannelRequest,
) -> Result<Conversation, AppError> {
    require_workspace_member(pool, workspace_id, actor).await?;
    let topic = optional_text(request.topic.as_deref(), topic_is_valid, "topic")?;
    let description = optional_text(
        request.description.as_deref(),
        description_is_valid,
        "description",
    )?;
    let fingerprint = crypto::command_fingerprint(
        "create_channel",
        &[
            workspace_id.as_uuid().as_bytes(),
            request.name.as_str().as_bytes(),
            db::visibility_to_db(request.visibility).as_bytes(),
            topic.as_deref().unwrap_or("").as_bytes(),
            description.as_deref().unwrap_or("").as_bytes(),
        ],
    );

    let id = ConversationId::generate();
    let mut tx = pool.begin().await?;
    if let Some(existing) = receipts::claim(
        &mut tx,
        actor,
        request.operation_id,
        ReceiptKind::CreateChannel,
        &fingerprint,
        ReceiptTarget::Conversation(id),
    )
    .await?
    {
        existing.ensure_same(ReceiptKind::CreateChannel, &fingerprint)?;
        let original = existing
            .conversation_id
            .map(ConversationId::from_uuid)
            .ok_or_else(|| AppError::internal("receipt has no conversation"))?;
        return conversation_dto(&mut tx, original).await;
    }

    let inserted = sqlx::query(
        "INSERT INTO conversations (id, workspace_id, kind, name, topic, description, visibility, created_by) \
         VALUES ($1, $2, 'channel', $3, $4, $5, $6, $7) ON CONFLICT (workspace_id, name) DO NOTHING",
    )
    .bind(id.as_uuid())
    .bind(workspace_id.as_uuid())
    .bind(request.name.as_str())
    .bind(&topic)
    .bind(&description)
    .bind(db::visibility_to_db(request.visibility))
    .bind(actor.as_uuid())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Err(AppError::new(
            ErrorCode::NameTaken,
            "a channel with that name already exists",
        ));
    }
    add_members(&mut tx, id, &[actor]).await?;
    let conversation = conversation_dto(&mut tx, id).await?;

    events::append_all(
        &mut tx,
        workspace_id,
        vec![PendingEvent {
            kind: "conversation_created",
            conversation_id: Some(id),
            operation_id: Some(request.operation_id),
            audience: Audience::Members,
            event: Event::ConversationCreated {
                conversation: conversation.clone(),
            },
        }],
    )
    .await?;
    tx.commit().await?;
    Ok(conversation)
}

/// Finds or creates the direct or group conversation for exactly the caller plus the people
/// named. Idempotent per participant set, including under concurrent identical requests: the
/// unique participant key decides a single winner, and only the winner appends
/// `ConversationCreated`.
pub async fn open_conversation(
    pool: &PgPool,
    actor: UserId,
    workspace_id: WorkspaceId,
    request: OpenConversationRequest,
) -> Result<Conversation, AppError> {
    require_workspace_member(pool, workspace_id, actor).await?;
    let participants = ParticipantSet::new(actor, request.participants)
        .map_err(|error| AppError::invalid(error.to_string()))?;
    let everyone: Vec<Uuid> = participants
        .users()
        .iter()
        .map(|user| user.as_uuid())
        .collect();
    let in_workspace: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workspace_members WHERE workspace_id = $1 AND user_id = ANY($2)",
    )
    .bind(workspace_id.as_uuid())
    .bind(&everyone)
    .fetch_one(pool)
    .await?;
    if usize::try_from(in_workspace).ok() != Some(everyone.len()) {
        return Err(AppError::invalid(
            "everyone in a conversation must belong to the workspace",
        ));
    }

    let key = participants.key();
    let fingerprint = crypto::command_fingerprint(
        "open_conversation",
        &[workspace_id.as_uuid().as_bytes(), key.as_bytes()],
    );

    let mut tx = pool.begin().await?;
    // Insert first and let the unique participant key arbitrate. A concurrent insert of the same
    // set makes this wait for its outcome; the loser then finds the winner's row. Nothing here
    // holds the workspace lock yet, so waiting is safe.
    let candidate = ConversationId::generate();
    let inserted = sqlx::query(
        "INSERT INTO conversations (id, workspace_id, kind, visibility, created_by, participant_key) \
         VALUES ($1, $2, $3, 'private', $4, $5) \
         ON CONFLICT (workspace_id, participant_key) WHERE participant_key IS NOT NULL DO NOTHING",
    )
    .bind(candidate.as_uuid())
    .bind(workspace_id.as_uuid())
    .bind(db::kind_to_db(participants.kind()))
    .bind(actor.as_uuid())
    .bind(&key)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    let id = if inserted {
        candidate
    } else {
        let existing: Uuid = sqlx::query_scalar(
            "SELECT id FROM conversations WHERE workspace_id = $1 AND participant_key = $2",
        )
        .bind(workspace_id.as_uuid())
        .bind(&key)
        .fetch_one(&mut *tx)
        .await?;
        ConversationId::from_uuid(existing)
    };

    if let Some(existing) = receipts::claim(
        &mut tx,
        actor,
        request.operation_id,
        ReceiptKind::OpenConversation,
        &fingerprint,
        ReceiptTarget::Conversation(id),
    )
    .await?
    {
        existing.ensure_same(ReceiptKind::OpenConversation, &fingerprint)?;
        if inserted {
            return Err(AppError::internal(
                "a repeated operation created a second conversation",
            ));
        }
        return conversation_dto(&mut tx, id).await;
    }

    if inserted {
        add_members(&mut tx, id, participants.users()).await?;
    }
    let conversation = conversation_dto(&mut tx, id).await?;
    if inserted {
        events::append_all(
            &mut tx,
            workspace_id,
            vec![PendingEvent {
                kind: "conversation_created",
                conversation_id: Some(id),
                operation_id: Some(request.operation_id),
                audience: Audience::Members,
                event: Event::ConversationCreated {
                    conversation: conversation.clone(),
                },
            }],
        )
        .await?;
    }
    tx.commit().await?;
    Ok(conversation)
}

/// Changes a channel's name, topic, or description. Renaming needs the creator or an admin; the
/// other fields need membership. Changes nothing (and appends nothing) if nothing differs.
pub async fn update_conversation(
    pool: &PgPool,
    actor: UserId,
    id: ConversationId,
    request: UpdateConversationRequest,
) -> Result<Conversation, AppError> {
    let (mut tx, loaded) = begin_visible(pool, actor, id, Lock::Exclusive).await?;
    if request.name.is_some() && !loaded.facts.can_rename() {
        return Err(forbidden("rename this channel"));
    }
    if (request.topic.is_some() || request.description.is_some())
        && !loaded.facts.can_edit_details()
    {
        return Err(forbidden("change this conversation's details"));
    }
    let topic = request
        .topic
        .as_deref()
        .map(|text| optional_text(Some(text), topic_is_valid, "topic"))
        .transpose()?;
    let description = request
        .description
        .as_deref()
        .map(|text| optional_text(Some(text), description_is_valid, "description"))
        .transpose()?;

    let name = request.name.as_ref().map(|name| name.as_str().to_owned());
    let current = &loaded.row;
    let changes = name
        .as_ref()
        .is_some_and(|name| current.name.as_ref() != Some(name))
        || topic.as_ref().is_some_and(|topic| *topic != current.topic)
        || description
            .as_ref()
            .is_some_and(|description| *description != current.description);
    if !changes {
        return conversation_dto(&mut tx, id).await;
    }

    let updated = sqlx::query(
        "UPDATE conversations SET \
             name = COALESCE($2, name), \
             topic = CASE WHEN $3 THEN $4 ELSE topic END, \
             description = CASE WHEN $5 THEN $6 ELSE description END \
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(&name)
    .bind(topic.is_some())
    .bind(topic.clone().flatten())
    .bind(description.is_some())
    .bind(description.clone().flatten())
    .execute(&mut *tx)
    .await;
    match updated {
        Ok(_) => {}
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            return Err(AppError::new(
                ErrorCode::NameTaken,
                "a channel with that name already exists",
            ));
        }
        Err(error) => return Err(error.into()),
    }

    let conversation = conversation_dto(&mut tx, id).await?;
    events::append_all(
        &mut tx,
        loaded.workspace_id,
        vec![PendingEvent {
            kind: "conversation_updated",
            conversation_id: Some(id),
            operation_id: None,
            audience: Audience::Members,
            event: Event::ConversationUpdated {
                conversation: conversation.clone(),
            },
        }],
    )
    .await?;
    tx.commit().await?;
    Ok(conversation)
}

/// Joins a public channel. Joining a channel the caller is already in changes nothing.
pub async fn join(
    pool: &PgPool,
    actor: UserId,
    id: ConversationId,
) -> Result<Conversation, AppError> {
    let (mut tx, loaded) = begin_visible(pool, actor, id, Lock::Exclusive).await?;
    if loaded.facts.is_member {
        return conversation_dto(&mut tx, id).await;
    }
    if !loaded.facts.can_join() {
        return Err(forbidden("join this channel"));
    }
    add_members(&mut tx, id, &[actor]).await?;
    let conversation = conversation_dto(&mut tx, id).await?;
    events::append_all(
        &mut tx,
        loaded.workspace_id,
        vec![PendingEvent {
            kind: "member_added",
            conversation_id: Some(id),
            operation_id: None,
            audience: Audience::Members,
            event: Event::MemberAdded {
                conversation: conversation.clone(),
                user_id: actor,
            },
        }],
    )
    .await?;
    tx.commit().await?;
    Ok(conversation)
}

/// Leaves a channel. Leaving one the caller is not in changes nothing.
pub async fn leave(pool: &PgPool, actor: UserId, id: ConversationId) -> Result<(), AppError> {
    let (mut tx, loaded) = begin_visible(pool, actor, id, Lock::Exclusive).await?;
    if !loaded.facts.is_member {
        return Ok(());
    }
    if !loaded.facts.can_leave() {
        return Err(forbidden("leave this conversation"));
    }
    remove_and_announce(&mut tx, &loaded, actor).await?;
    tx.commit().await?;
    Ok(())
}

/// Removes someone from a channel. Removing someone who is not in it changes nothing; removing
/// yourself is leaving.
pub async fn remove_member_command(
    pool: &PgPool,
    actor: UserId,
    id: ConversationId,
    target: UserId,
) -> Result<(), AppError> {
    if target == actor {
        return leave(pool, actor, id).await;
    }
    let (mut tx, loaded) = begin_visible(pool, actor, id, Lock::Exclusive).await?;
    if !loaded.facts.can_remove_members() {
        return Err(forbidden("remove people from this conversation"));
    }
    if remove_and_announce(&mut tx, &loaded, target).await? {
        tx.commit().await?;
    }
    Ok(())
}

/// Removes `user`, and appends `MemberRemoved` (to the remaining members and to `user`) and a
/// `ConversationUpdated` carrying the new member count. Returns whether `user` was a member.
async fn remove_and_announce(
    tx: &mut Transaction<'static, Postgres>,
    loaded: &Loaded,
    user: UserId,
) -> Result<bool, AppError> {
    if !remove_member(tx, loaded.id, user).await? {
        return Ok(false);
    }
    let conversation = conversation_dto(tx, loaded.id).await?;
    events::append_all(
        tx,
        loaded.workspace_id,
        vec![
            PendingEvent {
                kind: "member_removed",
                conversation_id: Some(loaded.id),
                operation_id: None,
                audience: Audience::MembersAndUser(user),
                event: Event::MemberRemoved {
                    conversation_id: loaded.id,
                    user_id: user,
                },
            },
            PendingEvent {
                kind: "conversation_updated",
                conversation_id: Some(loaded.id),
                operation_id: None,
                audience: Audience::Members,
                event: Event::ConversationUpdated { conversation },
            },
        ],
    )
    .await?;
    Ok(true)
}

/// Adds people to a channel. People already in it are ignored.
pub async fn add_members_command(
    pool: &PgPool,
    actor: UserId,
    id: ConversationId,
    request: AddMembersRequest,
) -> Result<Conversation, AppError> {
    if request.user_ids.len() > MAX_ADD_USERS {
        return Err(AppError::invalid(format!(
            "add at most {MAX_ADD_USERS} people at a time"
        )));
    }
    let (mut tx, loaded) = begin_visible(pool, actor, id, Lock::Exclusive).await?;
    if !loaded.facts.can_add_members() {
        return Err(forbidden("add people to this conversation"));
    }

    let mut wanted: Vec<UserId> = Vec::with_capacity(request.user_ids.len());
    for user in request.user_ids {
        if !wanted.contains(&user) {
            wanted.push(user);
        }
    }
    let ids: Vec<Uuid> = wanted.iter().map(|user| user.as_uuid()).collect();
    let in_workspace: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workspace_members WHERE workspace_id = $1 AND user_id = ANY($2)",
    )
    .bind(loaded.workspace_id.as_uuid())
    .bind(&ids)
    .fetch_one(&mut *tx)
    .await?;
    if usize::try_from(in_workspace).ok() != Some(wanted.len()) {
        return Err(AppError::invalid(
            "everyone added must belong to the workspace",
        ));
    }

    let before = i64::from(loaded.row.member_count);
    let added = add_members(&mut tx, id, &wanted).await?;
    let conversation = conversation_dto(&mut tx, id).await?;
    if !added.is_empty() {
        // Each event carries the conversation as it stood right after that person joined.
        let events_to_append = added
            .iter()
            .enumerate()
            .map(|(index, user)| {
                let mut as_of = conversation.clone();
                as_of.member_count = u32::try_from(before + index as i64 + 1).unwrap_or(u32::MAX);
                PendingEvent {
                    kind: "member_added",
                    conversation_id: Some(id),
                    operation_id: None,
                    audience: Audience::Members,
                    event: Event::MemberAdded {
                        conversation: as_of,
                        user_id: *user,
                    },
                }
            })
            .collect();
        events::append_all(&mut tx, loaded.workspace_id, events_to_append).await?;
    }
    tx.commit().await?;
    Ok(conversation)
}

/// Archives a channel: it stays readable but can no longer be posted to.
pub async fn archive(
    pool: &PgPool,
    actor: UserId,
    id: ConversationId,
) -> Result<Conversation, AppError> {
    set_archived(pool, actor, id, true).await
}

/// Reopens an archived channel.
pub async fn unarchive(
    pool: &PgPool,
    actor: UserId,
    id: ConversationId,
) -> Result<Conversation, AppError> {
    set_archived(pool, actor, id, false).await
}

async fn set_archived(
    pool: &PgPool,
    actor: UserId,
    id: ConversationId,
    archived: bool,
) -> Result<Conversation, AppError> {
    let (mut tx, loaded) = begin_visible(pool, actor, id, Lock::Exclusive).await?;
    let facts = &loaded.facts;
    // Asking for the state it is already in succeeds for anyone who could change it.
    let allowed = if archived {
        facts.can_archive()
    } else {
        facts.can_unarchive()
    };
    let already = loaded.facts.archived == archived;
    let could_manage = facts.can_archive() || facts.can_unarchive();
    if !(allowed || (already && could_manage)) {
        return Err(forbidden(if archived {
            "archive this conversation"
        } else {
            "unarchive this conversation"
        }));
    }
    if already {
        return conversation_dto(&mut tx, id).await;
    }

    sqlx::query("UPDATE conversations SET archived = $2 WHERE id = $1")
        .bind(id.as_uuid())
        .bind(archived)
        .execute(&mut *tx)
        .await?;
    let conversation = conversation_dto(&mut tx, id).await?;
    events::append_all(
        &mut tx,
        loaded.workspace_id,
        vec![PendingEvent {
            kind: "conversation_updated",
            conversation_id: Some(id),
            operation_id: None,
            audience: Audience::Members,
            event: Event::ConversationUpdated {
                conversation: conversation.clone(),
            },
        }],
    )
    .await?;
    tx.commit().await?;
    Ok(conversation)
}

/// Sets the caller's favorite flag. Appends `UserStateChanged` (to the caller only) when it
/// changes.
pub async fn set_favorite(
    pool: &PgPool,
    actor: UserId,
    id: ConversationId,
    favorite: bool,
) -> Result<ConversationUserState, AppError> {
    let (mut tx, loaded) = begin_visible(pool, actor, id, Lock::Shared).await?;
    if !loaded.facts.is_member {
        return Err(forbidden("favorite a conversation you are not in"));
    }
    let current: Option<bool> = sqlx::query_scalar(
        "SELECT favorite FROM conversation_user_state WHERE user_id = $1 AND conversation_id = $2 FOR UPDATE",
    )
    .bind(actor.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    let was = current.ok_or_else(|| forbidden("favorite a conversation you are not in"))?;
    if was != favorite {
        sqlx::query(
            "UPDATE conversation_user_state SET favorite = $3 WHERE user_id = $1 AND conversation_id = $2",
        )
        .bind(actor.as_uuid())
        .bind(id.as_uuid())
        .bind(favorite)
        .execute(&mut *tx)
        .await?;
    }
    let state = user_state(&mut tx, actor, id)
        .await?
        .ok_or_else(|| AppError::internal("state vanished"))?;
    if was != favorite {
        announce_state(&mut tx, &loaded, actor, &state).await?;
    }
    tx.commit().await?;
    Ok(state)
}

/// Moves the caller's read position forward to `up_to`, clamped to the conversation's newest
/// message. A position at or behind the current one changes nothing. Appends `UserStateChanged`
/// (to the caller only) when it advances.
pub async fn mark_read(
    pool: &PgPool,
    actor: UserId,
    id: ConversationId,
    up_to: EventSeq,
) -> Result<ConversationUserState, AppError> {
    let (mut tx, loaded) = begin_visible(pool, actor, id, Lock::Shared).await?;
    if !loaded.facts.is_member {
        return Err(forbidden("mark a conversation you are not in as read"));
    }
    let newest: i64 =
        sqlx::query_scalar("SELECT COALESCE(max(seq), 0) FROM messages WHERE conversation_id = $1")
            .bind(id.as_uuid())
            .fetch_one(&mut *tx)
            .await?;
    let target = up_to.get().min(newest);
    let current: Option<i64> = sqlx::query_scalar(
        "SELECT last_read FROM conversation_user_state WHERE user_id = $1 AND conversation_id = $2 FOR UPDATE",
    )
    .bind(actor.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    let last_read =
        current.ok_or_else(|| forbidden("mark a conversation you are not in as read"))?;
    let advanced = target > last_read;
    if advanced {
        sqlx::query(
            "UPDATE conversation_user_state SET last_read = $3 WHERE user_id = $1 AND conversation_id = $2",
        )
        .bind(actor.as_uuid())
        .bind(id.as_uuid())
        .bind(target)
        .execute(&mut *tx)
        .await?;
    }
    let state = user_state(&mut tx, actor, id)
        .await?
        .ok_or_else(|| AppError::internal("state vanished"))?;
    if advanced {
        announce_state(&mut tx, &loaded, actor, &state).await?;
    }
    tx.commit().await?;
    Ok(state)
}

async fn announce_state(
    tx: &mut Transaction<'static, Postgres>,
    loaded: &Loaded,
    user: UserId,
    state: &ConversationUserState,
) -> Result<(), AppError> {
    events::append_all(
        tx,
        loaded.workspace_id,
        vec![PendingEvent {
            kind: "user_state_changed",
            conversation_id: Some(loaded.id),
            operation_id: None,
            audience: Audience::User(user),
            event: Event::UserStateChanged {
                state: state.clone(),
            },
        }],
    )
    .await?;
    Ok(())
}

/// The id of the operation that produced a conversation, if the caller has a receipt for it.
pub async fn operation_conversation(
    conn: &mut PgConnection,
    user: UserId,
    operation: OperationId,
) -> Result<Option<Conversation>, AppError> {
    let Some(receipt) = receipts::find(conn, user, operation).await? else {
        return Ok(None);
    };
    match receipt.conversation_id {
        Some(id) => Ok(Some(
            conversation_dto(conn, ConversationId::from_uuid(id)).await?,
        )),
        None => Ok(None),
    }
}
