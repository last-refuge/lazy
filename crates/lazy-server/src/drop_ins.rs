//! Drop-ins (`llm-docs/DROP_INS.md`): calls with video in a conversation. The call itself runs on
//! the media library's SFU (`rtc-sfu`), a separate process; this server only decides who may join
//! and says so with a join token signed by a secret the two share.
//!
//! Who may join: anyone who can read the conversation, now. The token names the conversation's
//! room, the person (their user id, and their display name for others to see), and expires
//! after [`TOKEN_MINUTES`]: it is for joining, and is not checked again once in.
//!
//! Who is in a drop-in (D44): the SFU reports each room's participants
//! ([`rtc_proto::webhook`], `POST /v1/rtc/report`, signed with the same secret) when someone joins
//! or leaves and every 30 seconds while anyone is in it. The first report with someone in a room
//! starts a drop-in: a row in `drop_ins`, the "started a drop-in" notice (which in a direct
//! conversation notifies the other person), and the conversation's `drop_in`, announced with
//! `ConversationUpdated`. Later reports update both as people come and go; an empty one ends it,
//! and so does silence ([`DropInOptions::silent_after`]: the SFU restarted, or cannot reach us).
//!
//! Not here yet: removing someone who loses access while in a call (the SFU has no admin API).

use std::time::Duration;

use lazy_domain::{
    ConversationId, DropInNotice, MessageId, Notice, Timestamp, UserId, WorkspaceId,
};
use lazy_protocol::{ErrorCode, Event, JoinDropInResponse, LiveDropIn};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::app::AppState;
use crate::conversations::{self, Lock};
use crate::error::AppError;
use crate::events::{self, Audience, PendingEvent};
use crate::logging::Describe;
use crate::notices;

/// How long a join token is good for.
pub const TOKEN_MINUTES: i64 = 10;

/// Where the call server is, and the secret join tokens (and its reports) are signed with.
#[derive(Clone)]
pub struct DropInOptions {
    /// The SFU's signaling address as clients reach it (`wss://rtc.example.com/rtc`).
    pub url: String,
    pub secret: Vec<u8>,
    /// A live drop-in with no report for this long has ended (the SFU reports every 30 seconds).
    pub silent_after: Duration,
    /// How often live drop-ins are checked for silence.
    pub sweep_every: Duration,
}

impl std::fmt::Debug for DropInOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DropInOptions")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl DropInOptions {
    /// Checks the settings: a `ws://` or `wss://` address and a secret long enough.
    pub fn new(url: &str, secret: &str) -> Result<DropInOptions, String> {
        let url = url.trim().trim_end_matches('/').to_owned();
        if !(url.starts_with("wss://") || url.starts_with("ws://")) {
            return Err("the call server's address must start with wss:// (or ws://)".into());
        }
        let url = if url.ends_with(rtc_proto::SIGNAL_PATH) {
            url
        } else {
            format!("{url}{}", rtc_proto::SIGNAL_PATH)
        };
        if secret.len() < rtc_proto::token::MIN_SECRET_BYTES {
            return Err(format!(
                "the call server's secret must be at least {} characters",
                rtc_proto::token::MIN_SECRET_BYTES
            ));
        }
        Ok(DropInOptions {
            url,
            secret: secret.as_bytes().to_vec(),
            silent_after: Duration::from_secs(rtc_proto::webhook::HEARTBEAT_SECS * 3 + 10),
            sweep_every: Duration::from_secs(rtc_proto::webhook::HEARTBEAT_SECS),
        })
    }
}

/// `POST /v1/conversations/{id}/drop-in`.
pub async fn join(
    state: &AppState,
    actor: UserId,
    conversation: ConversationId,
) -> Result<JoinDropInResponse, AppError> {
    // Visible to the caller now, or not found.
    let (tx, _loaded) =
        conversations::begin_visible(&state.pool, actor, conversation, Lock::Shared).await?;
    drop(tx);
    let Some(options) = &state.integrations.drop_ins else {
        return Err(AppError::new(
            ErrorCode::Unavailable,
            "drop-ins aren't set up on this server",
        ));
    };
    let name: String = sqlx::query_scalar("SELECT display_name FROM users WHERE id = $1")
        .bind(actor.as_uuid())
        .fetch_one(&state.pool)
        .await?;
    let expires = OffsetDateTime::now_utc() + time::Duration::minutes(TOKEN_MINUTES);
    let claims = rtc_proto::Claims {
        room: conversation.to_string(),
        identity: actor.to_string(),
        name,
        exp: u64::try_from(expires.unix_timestamp()).unwrap_or(0),
    };
    let token = rtc_proto::token::sign(&claims, &options.secret)
        .map_err(|e| AppError::internal(format!("signing a drop-in token: {e}")))?;
    Ok(JoinDropInResponse {
        url: options.url.clone(),
        token,
    })
}

/// `POST /v1/rtc/report`: the SFU says who is in a room. `Ok(false)`: the signature or the time
/// is wrong (answered 401, so the SFU does not try again).
pub async fn report(state: &AppState, signature: &str, body: &[u8]) -> Result<bool, AppError> {
    let Some(options) = &state.integrations.drop_ins else {
        return Err(AppError::new(
            ErrorCode::Unavailable,
            "drop-ins aren't set up on this server",
        ));
    };
    let now_ms =
        u64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0);
    let report = match rtc_proto::webhook::verify(body, signature, &options.secret, now_ms) {
        Ok(report) => report,
        Err(error) => {
            let why = match error {
                rtc_proto::webhook::ReportError::Malformed => "malformed",
                rtc_proto::webhook::ReportError::BadSignature => "bad_signature",
                rtc_proto::webhook::ReportError::Stale => "stale",
                rtc_proto::webhook::ReportError::WeakSecret => "weak_secret",
            };
            tracing::warn!(error_code = why, "refused a room report");
            return Ok(false);
        }
    };
    // Rooms are conversations, people are users; anything else is not ours.
    let Ok(conversation) = report.room.parse::<ConversationId>() else {
        return Ok(true);
    };
    let mut people: Vec<UserId> = Vec::new();
    for participant in &report.participants {
        if let Ok(user) = participant.identity.parse::<UserId>()
            && !people.contains(&user)
        {
            people.push(user);
        }
    }
    let reported_at = i64::try_from(report.sent_at_ms).unwrap_or(i64::MAX);
    apply(&state.pool, conversation, &people, reported_at).await?;
    Ok(true)
}

/// A live drop-in's row.
#[derive(sqlx::FromRow)]
struct LiveRow {
    id: Uuid,
    message_id: Uuid,
    started_at: OffsetDateTime,
    participants: Vec<Uuid>,
    joined: Vec<Uuid>,
}

/// Brings a conversation's drop-in in line with who the SFU says is in its room.
pub(crate) async fn apply(
    pool: &PgPool,
    conversation: ConversationId,
    people: &[UserId],
    reported_at: i64,
) -> Result<(), AppError> {
    let mut tx = pool.begin().await?;
    // The conversation first (then the workspace, when events are appended), as every command.
    let workspace: Option<Uuid> = sqlx::query_scalar(
        "SELECT workspace_id FROM conversations WHERE id = $1 FOR NO KEY UPDATE",
    )
    .bind(conversation.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    let Some(workspace) = workspace.map(WorkspaceId::from_uuid) else {
        return Ok(());
    };
    // A report older than one already applied (to this drop-in or an earlier one) is stale.
    let newest: Option<i64> =
        sqlx::query_scalar("SELECT max(reported_at_ms) FROM drop_ins WHERE conversation_id = $1")
            .bind(conversation.as_uuid())
            .fetch_one(&mut *tx)
            .await?;
    if newest.is_some_and(|newest| reported_at < newest) {
        return Ok(());
    }
    // Only people who exist (an identity from elsewhere is ignored).
    let ids: Vec<Uuid> = people.iter().map(|user| user.as_uuid()).collect();
    let known: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM users WHERE id = ANY($1)")
        .bind(&ids)
        .fetch_all(&mut *tx)
        .await?;
    let people: Vec<Uuid> = ids.into_iter().filter(|id| known.contains(id)).collect();

    let live: Option<LiveRow> = sqlx::query_as(
        "SELECT id, message_id, started_at, participants, joined FROM drop_ins \
         WHERE conversation_id = $1 AND ended_at IS NULL",
    )
    .bind(conversation.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    match live {
        None if people.is_empty() => {}
        None => {
            let started_at = OffsetDateTime::now_utc();
            let starter = UserId::from_uuid(people[0]);
            let notice = DropInNotice {
                started_at: Timestamp::from(started_at),
                ended_at: None,
                participants: users(&people),
                joined: users(&people),
            };
            let message = notices::post(
                &mut tx,
                workspace,
                conversation,
                starter,
                Notice::DropIn(notice),
            )
            .await?;
            sqlx::query(
                "INSERT INTO drop_ins (id, conversation_id, message_id, started_by, started_at, \
                                       participants, joined, reported_at_ms, heard_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $6, $7, now())",
            )
            .bind(Uuid::new_v4())
            .bind(conversation.as_uuid())
            .bind(message.id.as_uuid())
            .bind(starter.as_uuid())
            .bind(started_at)
            .bind(&people)
            .bind(reported_at)
            .execute(&mut *tx)
            .await?;
            let live = LiveDropIn {
                message_id: message.id,
                started_at: Timestamp::from(started_at),
                participants: users(&people),
            };
            announce(&mut tx, workspace, conversation, Some(live)).await?;
        }
        Some(row) if people.is_empty() => {
            sqlx::query("UPDATE drop_ins SET reported_at_ms = $2, heard_at = now() WHERE id = $1")
                .bind(row.id)
                .bind(reported_at)
                .execute(&mut *tx)
                .await?;
            end(&mut tx, workspace, conversation, row).await?;
        }
        Some(row) => {
            let mut joined = row.joined.clone();
            for person in &people {
                if !joined.contains(person) {
                    joined.push(*person);
                }
            }
            sqlx::query(
                "UPDATE drop_ins SET participants = $2, joined = $3, reported_at_ms = $4, \
                                     heard_at = now() WHERE id = $1",
            )
            .bind(row.id)
            .bind(&people)
            .bind(&joined)
            .bind(reported_at)
            .execute(&mut *tx)
            .await?;
            if people != row.participants {
                let message_id = MessageId::from_uuid(row.message_id);
                let live = LiveDropIn {
                    message_id,
                    started_at: Timestamp::from(row.started_at),
                    participants: users(&people),
                };
                announce(&mut tx, workspace, conversation, Some(live)).await?;
                let notice = DropInNotice {
                    started_at: Timestamp::from(row.started_at),
                    ended_at: None,
                    participants: users(&people),
                    joined: users(&joined),
                };
                notices::update(
                    &mut tx,
                    workspace,
                    conversation,
                    message_id,
                    Notice::DropIn(notice),
                )
                .await?;
            }
        }
    }
    tx.commit().await?;
    Ok(())
}

fn users(ids: &[Uuid]) -> Vec<UserId> {
    ids.iter().copied().map(UserId::from_uuid).collect()
}

/// Ends a live drop-in: its row, the conversation's `drop_in`, and its notice ("ended after").
async fn end(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    conversation: ConversationId,
    row: LiveRow,
) -> Result<(), AppError> {
    let ended_at = OffsetDateTime::now_utc();
    sqlx::query("UPDATE drop_ins SET ended_at = $2, participants = '{}' WHERE id = $1")
        .bind(row.id)
        .bind(ended_at)
        .execute(&mut *conn)
        .await?;
    announce(conn, workspace, conversation, None).await?;
    let notice = DropInNotice {
        started_at: Timestamp::from(row.started_at),
        ended_at: Some(Timestamp::from(ended_at)),
        participants: Vec::new(),
        joined: users(&row.joined),
    };
    notices::update(
        conn,
        workspace,
        conversation,
        MessageId::from_uuid(row.message_id),
        Notice::DropIn(notice),
    )
    .await
}

/// Sets the conversation's `drop_in` and tells its members (`ConversationUpdated`).
async fn announce(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    conversation: ConversationId,
    live: Option<LiveDropIn>,
) -> Result<(), AppError> {
    sqlx::query("UPDATE conversations SET drop_in = $2 WHERE id = $1")
        .bind(conversation.as_uuid())
        .bind(live.as_ref().map(Json))
        .execute(&mut *conn)
        .await?;
    let updated = conversations::conversation_dto(conn, conversation).await?;
    events::append_all(
        conn,
        workspace,
        vec![PendingEvent {
            kind: "conversation_updated",
            conversation_id: Some(conversation),
            operation_id: None,
            audience: Audience::Members,
            event: Event::ConversationUpdated {
                conversation: updated,
            },
        }],
    )
    .await?;
    Ok(())
}

/// Ends every live drop-in not heard of for `silent_after`. Returns how many ended.
pub async fn end_silent(pool: &PgPool, silent_after: Duration) -> Result<usize, AppError> {
    let silent: Vec<Uuid> = sqlx::query_scalar(
        "SELECT conversation_id FROM drop_ins \
         WHERE ended_at IS NULL AND heard_at < now() - make_interval(secs => $1)",
    )
    .bind(silent_after.as_secs_f64())
    .fetch_all(pool)
    .await?;
    let mut ended = 0;
    for conversation in silent {
        let mut tx = pool.begin().await?;
        let workspace: Option<Uuid> = sqlx::query_scalar(
            "SELECT workspace_id FROM conversations WHERE id = $1 FOR NO KEY UPDATE",
        )
        .bind(conversation)
        .fetch_optional(&mut *tx)
        .await?;
        // Checked again under the lock: a report may have arrived since.
        let row: Option<LiveRow> = sqlx::query_as(
            "SELECT id, message_id, started_at, participants, joined FROM drop_ins \
             WHERE conversation_id = $1 AND ended_at IS NULL \
               AND heard_at < now() - make_interval(secs => $2)",
        )
        .bind(conversation)
        .bind(silent_after.as_secs_f64())
        .fetch_optional(&mut *tx)
        .await?;
        if let (Some(workspace), Some(row)) = (workspace, row) {
            tracing::info!(%conversation, "a drop-in went silent; ending it");
            end(
                &mut tx,
                WorkspaceId::from_uuid(workspace),
                ConversationId::from_uuid(conversation),
                row,
            )
            .await?;
            tx.commit().await?;
            ended += 1;
        }
    }
    Ok(ended)
}

/// Checks for silent drop-ins every `sweep_every` until `shutdown`.
pub(crate) async fn sweep(pool: PgPool, options: DropInOptions, shutdown: CancellationToken) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(options.sweep_every) => {}
        }
        if let Err(error) = end_silent(&pool, options.silent_after).await {
            tracing::warn!(error = %error.describe(), "checking for silent drop-ins failed");
        }
    }
}
