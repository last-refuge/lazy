//! Encrypted integration credentials (llm-docs/DECISIONS.md, D31).
//!
//! A credential is a secret an integration holds for a workspace: an app's private key, an access
//! token, a signing secret. It is stored in `integration_credentials` as XChaCha20-Poly1305
//! ciphertext under a **server key** that is configured outside the database
//! (`LAZY_CREDENTIALS_KEY`, 32 bytes as 64 hex digits), so a copy of the database alone reveals
//! nothing. The row's id, workspace and kind are the associated data: a ciphertext moved to another
//! row, another workspace, or another kind does not open.
//!
//! # Keys and rotation
//!
//! [`CredentialKeys`] holds the current key, which seals everything new, and any number of
//! previous keys (`LAZY_CREDENTIALS_OLD_KEYS`), which only open. Each row records which key sealed
//! it by an id (the first 8 hex digits of the key's SHA-256), so opening never guesses. To rotate:
//! make the new key current, move the old one to the previous keys, run
//! `lazy-server rewrap-credentials` (which re-seals every row under the current key, [`rewrap_all`]),
//! then drop the old key. Without a current key the server runs, and everything that needs a
//! credential answers `unavailable` (integrations are off), so nothing about an existing
//! deployment changes until a key is set.
//!
//! # Holding a secret
//!
//! [`Secret`] wraps the bytes: its `Debug` is `Secret(<redacted>)`, it has no `Display` or
//! `Serialize`, and it is zeroed when dropped. Nothing here logs a secret, a key, or a ciphertext.

use std::fmt;

use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, Generate, Key, KeyInit, Nonce, Payload};
use lazy_domain::WorkspaceId;
use lazy_protocol::ErrorCode;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use crate::error::AppError;

/// Bound into every ciphertext, so a credential cannot be confused with anything else sealed by
/// the same key.
const CONTEXT: &[u8] = b"lazy-credential-v1";
const NONCE_LEN: usize = 24;

/// Bytes that must not be seen: zeroed on drop, redacted in `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Zeroizing<Vec<u8>>);

impl Secret {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub fn from_text(text: &str) -> Self {
        Self::new(text.as_bytes().to_vec())
    }

    /// The bytes, for the one place that uses them (signing a request, sending a token).
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// The bytes as UTF-8 text, if they are.
    pub fn expose_text(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// One server key.
#[derive(Clone)]
struct ServerKey {
    id: String,
    cipher: XChaCha20Poly1305,
}

impl ServerKey {
    fn from_bytes(mut bytes: [u8; 32]) -> Self {
        let digest = Sha256::digest(bytes);
        let id = digest[..4]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let key = Key::<XChaCha20Poly1305>::from(bytes);
        bytes.zeroize();
        Self {
            id,
            cipher: XChaCha20Poly1305::new(&key),
        }
    }
}

/// Why a configured key was refused. Says what is wrong with it, never what it is.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("a credentials key must be 64 hex digits (32 bytes; `openssl rand -hex 32` makes one)")]
    Malformed,
    #[error("the same credentials key is configured twice")]
    Duplicate,
}

/// The server's credential keys: the current one (if any) and previous ones that only open.
#[derive(Clone, Default)]
pub struct CredentialKeys {
    current: Option<ServerKey>,
    previous: Vec<ServerKey>,
}

impl fmt::Debug for CredentialKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialKeys")
            .field("current", &self.current.as_ref().map(|k| k.id.as_str()))
            .field(
                "previous",
                &self
                    .previous
                    .iter()
                    .map(|k| k.id.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

fn parse_key(text: &str) -> Result<ServerKey, KeyError> {
    let text = text.trim();
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(KeyError::Malformed);
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).map_err(|_| KeyError::Malformed)?;
    }
    Ok(ServerKey::from_bytes(bytes))
}

impl CredentialKeys {
    /// No keys: integrations that need credentials are unavailable.
    pub fn none() -> Self {
        Self::default()
    }

    /// From configuration: the current key (64 hex digits) and previous keys separated by commas.
    /// Empty text is no key.
    pub fn parse(current: Option<&str>, previous: Option<&str>) -> Result<Self, KeyError> {
        let current = current
            .filter(|text| !text.trim().is_empty())
            .map(parse_key)
            .transpose()?;
        let previous = previous
            .unwrap_or_default()
            .split(',')
            .filter(|text| !text.trim().is_empty())
            .map(parse_key)
            .collect::<Result<Vec<_>, _>>()?;
        let mut ids: Vec<&str> = current
            .iter()
            .chain(&previous)
            .map(|key| key.id.as_str())
            .collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() != count {
            return Err(KeyError::Duplicate);
        }
        Ok(Self { current, previous })
    }

    /// From raw key bytes, for tests and tools.
    pub fn from_bytes(current: [u8; 32], previous: &[[u8; 32]]) -> Self {
        Self {
            current: Some(ServerKey::from_bytes(current)),
            previous: previous
                .iter()
                .copied()
                .map(ServerKey::from_bytes)
                .collect(),
        }
    }

    /// Whether credentials can be stored: a current key is configured.
    pub fn enabled(&self) -> bool {
        self.current.is_some()
    }

    /// The id of the current key, for logs and the rewrap report.
    pub fn current_id(&self) -> Option<&str> {
        self.current.as_ref().map(|key| key.id.as_str())
    }

    fn current(&self) -> Result<&ServerKey, AppError> {
        self.current.as_ref().ok_or_else(unavailable)
    }

    fn by_id(&self, id: &str) -> Option<&ServerKey> {
        self.current
            .iter()
            .chain(&self.previous)
            .find(|key| key.id == id)
    }
}

fn unavailable() -> AppError {
    AppError::new(
        ErrorCode::Unavailable,
        "integrations are not set up on this server (it has no credentials key)",
    )
}

/// What a sealed credential is bound to.
fn associated_data(id: Uuid, workspace: WorkspaceId, kind: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(CONTEXT.len() + 32 + kind.len() + 2);
    aad.extend_from_slice(CONTEXT);
    aad.push(0);
    aad.extend_from_slice(id.as_bytes());
    aad.extend_from_slice(workspace.as_uuid().as_bytes());
    aad.push(0);
    aad.extend_from_slice(kind.as_bytes());
    aad
}

fn seal(
    key: &ServerKey,
    id: Uuid,
    workspace: WorkspaceId,
    kind: &str,
    secret: &Secret,
) -> Result<(Vec<u8>, Vec<u8>), AppError> {
    let nonce = Nonce::<XChaCha20Poly1305>::generate();
    let aad = associated_data(id, workspace, kind);
    let ciphertext = key
        .cipher
        .encrypt(
            &nonce,
            Payload {
                msg: secret.expose(),
                aad: &aad,
            },
        )
        .map_err(|_| AppError::internal("sealing a credential failed"))?;
    Ok((nonce.to_vec(), ciphertext))
}

fn open(
    key: &ServerKey,
    id: Uuid,
    workspace: WorkspaceId,
    kind: &str,
    nonce: &[u8],
    ciphertext: &[u8],
) -> Result<Secret, AppError> {
    if nonce.len() != NONCE_LEN {
        return Err(AppError::internal("a stored credential is damaged"));
    }
    let nonce = Nonce::<XChaCha20Poly1305>::try_from(nonce)
        .map_err(|_| AppError::internal("a stored credential is damaged"))?;
    let aad = associated_data(id, workspace, kind);
    key.cipher
        .decrypt(
            &nonce,
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map(Secret::new)
        .map_err(|_| AppError::internal("a stored credential does not open with its key"))
}

/// A credential row as read: workspace, kind, key id, nonce, ciphertext (the last three absent
/// once revoked).
type SealedRow = (
    Uuid,
    String,
    Option<String>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
);

/// A stored credential's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CredentialId(pub Uuid);

/// Seals `secret` under the current key and stores it for `workspace` as `kind`. Runs on the
/// caller's connection so it can join the transaction that installs the integration.
pub async fn store(
    conn: &mut PgConnection,
    keys: &CredentialKeys,
    workspace: WorkspaceId,
    kind: &str,
    secret: &Secret,
) -> Result<CredentialId, AppError> {
    let key = keys.current()?;
    let id = Uuid::now_v7();
    let (nonce, ciphertext) = seal(key, id, workspace, kind, secret)?;
    sqlx::query(
        "INSERT INTO integration_credentials (id, workspace_id, kind, key_id, nonce, ciphertext) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(workspace.as_uuid())
    .bind(kind)
    .bind(&key.id)
    .bind(&nonce)
    .bind(&ciphertext)
    .execute(conn)
    .await?;
    Ok(CredentialId(id))
}

/// Replaces a credential's secret (a refreshed token, a new signing secret) under the current key.
/// A revoked or unknown credential is `not_found`.
pub async fn replace(
    conn: &mut PgConnection,
    keys: &CredentialKeys,
    id: CredentialId,
    secret: &Secret,
) -> Result<(), AppError> {
    let key = keys.current()?;
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT workspace_id, kind FROM integration_credentials \
         WHERE id = $1 AND revoked_at IS NULL FOR UPDATE",
    )
    .bind(id.0)
    .fetch_optional(&mut *conn)
    .await?;
    let (workspace, kind) = row.ok_or_else(|| AppError::not_found("credential not found"))?;
    let (nonce, ciphertext) = seal(key, id.0, WorkspaceId::from_uuid(workspace), &kind, secret)?;
    sqlx::query(
        "UPDATE integration_credentials \
         SET key_id = $2, nonce = $3, ciphertext = $4, rotated_at = now() WHERE id = $1",
    )
    .bind(id.0)
    .bind(&key.id)
    .bind(&nonce)
    .bind(&ciphertext)
    .execute(conn)
    .await?;
    Ok(())
}

/// Opens a credential. A revoked or unknown one is `not_found`; one sealed by a key the server no
/// longer has, or with no key configured, is `unavailable`; one that does not open is internal
/// (damage, or a row tampered with).
pub async fn load(
    conn: &mut PgConnection,
    keys: &CredentialKeys,
    id: CredentialId,
) -> Result<Secret, AppError> {
    if !keys.enabled() {
        return Err(unavailable());
    }
    let row: Option<SealedRow> = sqlx::query_as(
        "SELECT workspace_id, kind, key_id, nonce, ciphertext FROM integration_credentials \
             WHERE id = $1",
    )
    .bind(id.0)
    .fetch_optional(conn)
    .await?;
    let Some((workspace, kind, Some(key_id), Some(nonce), Some(ciphertext))) = row else {
        return Err(AppError::not_found("credential not found"));
    };
    let key = keys.by_id(&key_id).ok_or_else(|| {
        AppError::new(
            ErrorCode::Unavailable,
            "a credential was sealed with a key this server no longer has",
        )
    })?;
    open(
        key,
        id.0,
        WorkspaceId::from_uuid(workspace),
        &kind,
        &nonce,
        &ciphertext,
    )
}

/// Revokes a credential: its ciphertext is erased and the row stays (for audit). Idempotent.
pub async fn revoke(conn: &mut PgConnection, id: CredentialId) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE integration_credentials \
         SET key_id = NULL, nonce = NULL, ciphertext = NULL, revoked_at = now() \
         WHERE id = $1 AND revoked_at IS NULL",
    )
    .bind(id.0)
    .execute(conn)
    .await?;
    Ok(())
}

/// What a rewrap did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RewrapReport {
    /// Re-sealed under the current key.
    pub rewrapped: u64,
    /// Already under the current key.
    pub current: u64,
    /// Sealed by a key the server does not have: left as they are, and they cannot be opened.
    pub missing_key: u64,
}

/// Re-seals every live credential that is not under the current key, one row per transaction, so
/// a rotation can drop the old key afterwards. Safe to run again, and alongside a running server.
pub async fn rewrap_all(pool: &PgPool, keys: &CredentialKeys) -> Result<RewrapReport, AppError> {
    let key = keys.current()?;
    let mut report = RewrapReport::default();
    let current: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM integration_credentials WHERE revoked_at IS NULL AND key_id = $1",
    )
    .bind(&key.id)
    .fetch_one(pool)
    .await?;
    report.current = u64::try_from(current).unwrap_or(0);
    let stale: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM integration_credentials WHERE revoked_at IS NULL AND key_id <> $1 \
         ORDER BY id",
    )
    .bind(&key.id)
    .fetch_all(pool)
    .await?;
    for id in stale {
        let mut tx = pool.begin().await?;
        let row: Option<SealedRow> = sqlx::query_as(
            "SELECT workspace_id, kind, key_id, nonce, ciphertext \
                 FROM integration_credentials WHERE id = $1 FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((workspace, kind, Some(key_id), Some(nonce), Some(ciphertext))) = row else {
            continue; // revoked meanwhile
        };
        if key_id == key.id {
            report.current += 1;
            continue; // re-sealed meanwhile
        }
        let Some(old) = keys.by_id(&key_id) else {
            report.missing_key += 1;
            continue;
        };
        let workspace = WorkspaceId::from_uuid(workspace);
        let secret = open(old, id, workspace, &kind, &nonce, &ciphertext)?;
        let (nonce, ciphertext) = seal(key, id, workspace, &kind, &secret)?;
        sqlx::query(
            "UPDATE integration_credentials \
             SET key_id = $2, nonce = $3, ciphertext = $4, rotated_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(&key.id)
        .bind(&nonce)
        .bind(&ciphertext)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        report.rewrapped += 1;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_64_hex_digits_and_never_shown() {
        let hex = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let keys = CredentialKeys::parse(Some(hex), None).unwrap();
        assert!(keys.enabled());
        let shown = format!("{keys:?}");
        assert!(
            !shown.contains(hex) && !shown.contains("00112233"),
            "{shown}"
        );
        assert_eq!(keys.current_id().unwrap().len(), 8);
        for bad in ["", "abc", &hex[..62], &format!("{}zz", &hex[..62])] {
            if bad.is_empty() {
                assert!(!CredentialKeys::parse(Some(bad), None).unwrap().enabled());
            } else {
                assert_eq!(
                    CredentialKeys::parse(Some(bad), None).unwrap_err(),
                    KeyError::Malformed
                );
            }
        }
        assert_eq!(
            CredentialKeys::parse(Some(hex), Some(hex)).unwrap_err(),
            KeyError::Duplicate
        );
        assert!(!CredentialKeys::none().enabled());
    }

    #[test]
    fn a_sealed_secret_opens_only_for_its_own_row_workspace_kind_and_key() {
        let keys = CredentialKeys::from_bytes([7; 32], &[]);
        let key = keys.current().unwrap();
        let (id, ws) = (Uuid::now_v7(), WorkspaceId::generate());
        let secret = Secret::from_text("ghs_token");
        let (nonce, ct) = seal(key, id, ws, "github.token", &secret).unwrap();
        assert!(!ct.windows(9).any(|w| w == b"ghs_token"));
        assert_eq!(
            open(key, id, ws, "github.token", &nonce, &ct).unwrap(),
            secret
        );
        assert!(open(key, Uuid::now_v7(), ws, "github.token", &nonce, &ct).is_err());
        assert!(
            open(
                key,
                id,
                WorkspaceId::generate(),
                "github.token",
                &nonce,
                &ct
            )
            .is_err()
        );
        assert!(open(key, id, ws, "github.key", &nonce, &ct).is_err());
        let other = CredentialKeys::from_bytes([8; 32], &[]);
        assert!(
            open(
                other.current().unwrap(),
                id,
                ws,
                "github.token",
                &nonce,
                &ct
            )
            .is_err()
        );
        let mut flipped = ct.clone();
        flipped[0] ^= 1;
        assert!(open(key, id, ws, "github.token", &nonce, &flipped).is_err());
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
    }
}
