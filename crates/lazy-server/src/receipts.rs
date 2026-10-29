//! Idempotency records for commands that carry an operation id: sending a message, editing one,
//! creating a channel, and opening a conversation.
//!
//! A command claims `(user, operation)` early in its transaction. A repeat of the same command
//! finds the receipt and returns the original result; the same operation id used for anything
//! different is a conflict. Two simultaneous repeats serialize on the claim: the second waits for
//! the first's outcome.

use lazy_domain::{ConversationId, GroupId, MessageId, OperationId, UserId};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::AppError;

/// Which command a receipt belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptKind {
    SendMessage,
    EditMessage,
    CreateChannel,
    OpenConversation,
    CreateGroup,
}

impl ReceiptKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::SendMessage => "send_message",
            Self::EditMessage => "edit_message",
            Self::CreateChannel => "create_channel",
            Self::OpenConversation => "open_conversation",
            Self::CreateGroup => "create_group",
        }
    }
}

/// What the command produced.
#[derive(Debug, Clone, Copy)]
pub enum ReceiptTarget {
    Message(MessageId),
    Conversation(ConversationId),
    Group(GroupId),
}

/// A stored receipt.
#[derive(Debug, sqlx::FromRow)]
pub struct Receipt {
    pub kind: String,
    request_hash: Vec<u8>,
    pub message_id: Option<Uuid>,
    /// For an edit: the revision it produced, or found if it changed nothing.
    pub message_revision: Option<i32>,
    pub conversation_id: Option<Uuid>,
    /// For creating a group. Not a foreign key: the receipt outlives the group.
    pub group_id: Option<Uuid>,
}

impl Receipt {
    /// Succeeds if this receipt is for the same command with the same content, and is a
    /// `conflict` otherwise.
    pub fn ensure_same(&self, kind: ReceiptKind, fingerprint: &[u8; 32]) -> Result<(), AppError> {
        self.ensure_same_any(kind, std::slice::from_ref(fingerprint))
    }

    /// Like [`Self::ensure_same`], accepting any of several fingerprints for the same request.
    /// Sends use this to recognize a receipt written before documents changed version.
    pub fn ensure_same_any(
        &self,
        kind: ReceiptKind,
        fingerprints: &[[u8; 32]],
    ) -> Result<(), AppError> {
        if self.kind == kind.as_str()
            && fingerprints
                .iter()
                .any(|fingerprint| self.request_hash == fingerprint[..])
        {
            Ok(())
        } else {
            Err(AppError::conflict(
                "this operation id was already used for something different",
            ))
        }
    }
}

/// Claims the operation for `user`. Returns `None` if this call claimed it, or the existing
/// receipt if the operation was already claimed (waiting for any in-flight claimant to finish).
pub async fn claim(
    conn: &mut PgConnection,
    user: UserId,
    operation: OperationId,
    kind: ReceiptKind,
    fingerprint: &[u8; 32],
    target: ReceiptTarget,
) -> Result<Option<Receipt>, AppError> {
    let (message_id, conversation_id, group_id) = match target {
        ReceiptTarget::Message(id) => (Some(id.as_uuid()), None, None),
        ReceiptTarget::Conversation(id) => (None, Some(id.as_uuid()), None),
        ReceiptTarget::Group(id) => (None, None, Some(id.as_uuid())),
    };
    let claimed = sqlx::query(
        "INSERT INTO operation_receipts (user_id, operation_id, request_hash, kind, message_id, conversation_id, group_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (user_id, operation_id) DO NOTHING",
    )
    .bind(user.as_uuid())
    .bind(operation.as_uuid())
    .bind(&fingerprint[..])
    .bind(kind.as_str())
    .bind(message_id)
    .bind(conversation_id)
    .bind(group_id)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        == 1;
    if claimed {
        return Ok(None);
    }
    find(conn, user, operation)
        .await?
        .map(Some)
        .ok_or_else(|| AppError::internal("operation receipt vanished"))
}

/// The receipt for an operation, if one exists for this user.
pub async fn find(
    conn: &mut PgConnection,
    user: UserId,
    operation: OperationId,
) -> Result<Option<Receipt>, AppError> {
    let receipt = sqlx::query_as(
        "SELECT kind, request_hash, message_id, message_revision, conversation_id, group_id FROM operation_receipts \
         WHERE user_id = $1 AND operation_id = $2",
    )
    .bind(user.as_uuid())
    .bind(operation.as_uuid())
    .fetch_optional(conn)
    .await?;
    Ok(receipt)
}

/// Records the revision an edit produced (or found, if it changed nothing), so that a repeat of
/// the same request can return its original result later.
pub async fn set_message_revision(
    conn: &mut PgConnection,
    user: UserId,
    operation: OperationId,
    revision: u32,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE operation_receipts SET message_revision = $3 WHERE user_id = $1 AND operation_id = $2",
    )
    .bind(user.as_uuid())
    .bind(operation.as_uuid())
    .bind(i32::try_from(revision).unwrap_or(i32::MAX))
    .execute(conn)
    .await?;
    Ok(())
}
