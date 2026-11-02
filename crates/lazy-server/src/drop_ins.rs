//! Drop-ins (`llm-docs/DROP_INS.md`): calls with video in a conversation. The call itself runs on
//! the media library's SFU (`rtc-sfu`), a separate process; this server only decides who may join
//! and says so with a join token signed by a secret the two share.
//!
//! Who may join: anyone who can read the conversation, now. The token names the conversation's
//! room, the person (their user id, and their display name for others to see), and expires
//! after [`TOKEN_MINUTES`]: it is for joining, and is not checked again once in.
//!
//! Not here yet: who is in a drop-in, seen from outside it (the SFU's webhooks), "drop-in started"
//! posts, and removing someone who loses access while in a call.

use lazy_domain::{ConversationId, UserId};
use lazy_protocol::{ErrorCode, JoinDropInResponse};
use time::OffsetDateTime;

use crate::app::AppState;
use crate::conversations::{self, Lock};
use crate::error::AppError;

/// How long a join token is good for.
pub const TOKEN_MINUTES: i64 = 10;

/// Where the call server is, and the secret join tokens are signed with.
#[derive(Clone)]
pub struct DropInOptions {
    /// The SFU's signaling address as clients reach it (`wss://rtc.example.com/rtc`).
    pub url: String,
    pub secret: Vec<u8>,
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
