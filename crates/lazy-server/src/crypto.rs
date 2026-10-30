//! Password hashing, session tokens, and request fingerprints.

use std::sync::Arc;

use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Argon2, Params};
use lazy_domain::ConversationId;
use lazy_protocol::{Password, SessionToken};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::config::PasswordCost;
use crate::error::AppError;

/// Shortest and longest accepted passwords, in bytes.
pub const PASSWORD_MIN_BYTES: usize = 8;
pub const PASSWORD_MAX_BYTES: usize = 256;

/// Prefix on session tokens, so leaked tokens are recognizable by secret scanners.
const TOKEN_PREFIX: &str = "lzy_";
const TOKEN_RANDOM_BYTES: usize = 32;
/// Hashes running at once. Each uses `memory_kib` of memory, so this bounds the cost of a flood
/// of sign-in attempts.
const MAX_CONCURRENT_HASHES: usize = 4;

/// Rejects passwords outside the accepted length.
pub fn check_password_policy(password: &Password) -> Result<(), AppError> {
    let len = password.expose().len();
    if len < PASSWORD_MIN_BYTES {
        return Err(AppError::invalid(format!(
            "password must be at least {PASSWORD_MIN_BYTES} bytes"
        )));
    }
    if len > PASSWORD_MAX_BYTES {
        return Err(AppError::invalid(format!(
            "password must be at most {PASSWORD_MAX_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Hashes and verifies passwords with Argon2id off the async executor threads.
#[derive(Clone)]
pub struct PasswordService {
    inner: Arc<PasswordInner>,
}

struct PasswordInner {
    params: Params,
    /// A valid hash that matches no real password, verified when a username is unknown so that
    /// sign-in takes about as long either way.
    decoy_hash: String,
    permits: Semaphore,
}

impl PasswordService {
    pub fn new(cost: PasswordCost) -> Result<Self, AppError> {
        let params = Params::new(cost.memory_kib, cost.iterations, cost.parallelism, None)
            .map_err(|e| AppError::internal(format!("invalid password hashing cost: {e}")))?;
        let mut decoy = [0u8; TOKEN_RANDOM_BYTES];
        fill_random(&mut decoy)?;
        let decoy_hash = hash_blocking(&params, hex(&decoy).as_bytes())?;
        Ok(Self {
            inner: Arc::new(PasswordInner {
                params,
                decoy_hash,
                permits: Semaphore::new(MAX_CONCURRENT_HASHES),
            }),
        })
    }

    /// Returns a PHC-format Argon2id hash with a fresh random salt.
    pub async fn hash(&self, password: &Password) -> Result<String, AppError> {
        let _permit = self.permit().await?;
        let params = self.inner.params.clone();
        let bytes = password.expose().as_bytes().to_vec();
        run_blocking(move || hash_blocking(&params, &bytes)).await?
    }

    /// Checks a password against a stored hash. With `None` (unknown user) it verifies against a
    /// decoy and returns `false`.
    pub async fn verify(
        &self,
        password: &Password,
        stored: Option<&str>,
    ) -> Result<bool, AppError> {
        let _permit = self.permit().await?;
        let params = self.inner.params.clone();
        let bytes = password.expose().as_bytes().to_vec();
        let hash = stored.unwrap_or(&self.inner.decoy_hash).to_owned();
        let matched = stored.is_some();
        let ok = run_blocking(move || {
            Argon2::from(params)
                .verify_password(&bytes, hash.as_str())
                .is_ok()
        })
        .await?;
        Ok(ok && matched)
    }

    async fn permit(&self) -> Result<tokio::sync::SemaphorePermit<'_>, AppError> {
        self.inner
            .permits
            .acquire()
            .await
            .map_err(|_| AppError::internal("password hashing is shut down"))
    }
}

async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, AppError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| AppError::internal(format!("blocking task failed: {e}")))
}

fn hash_blocking(params: &Params, password: &[u8]) -> Result<String, AppError> {
    Argon2::from(params.clone())
        .hash_password(password)
        .map(|hash| hash.to_string())
        .map_err(|e| AppError::internal(format!("password hashing failed: {e}")))
}

/// A new random session token and the SHA-256 that is stored in the database.
pub fn new_session_token() -> Result<(SessionToken, [u8; 32]), AppError> {
    let mut random = [0u8; TOKEN_RANDOM_BYTES];
    fill_random(&mut random)?;
    let token = format!("{TOKEN_PREFIX}{}", hex(&random));
    let hash = token_hash(&token);
    Ok((SessionToken::new(token), hash))
}

/// A new webhook secret: 256 random bits as 64 lowercase hex characters, and the SHA-256 hash that
/// is stored in its place. The secret is shown once, in the webhook's address, and never kept.
pub fn new_webhook_secret() -> Result<(String, [u8; 32]), AppError> {
    let mut random = [0u8; 32];
    fill_random(&mut random)?;
    let secret = hex(&random);
    let hash = webhook_secret_hash(&secret);
    Ok((secret, hash))
}

/// A new invitation code: 100 random bits as 20 characters of an alphabet without lookalikes
/// (`lazy_domain::workspace::invite_code_from_entropy`), and the SHA-256 hash that is stored in
/// its place. The code is shown once, in the response that creates the invitation, and a person
/// reads it aloud, types it, and pastes it, which a longer code of mixed case would make hard.
/// 100 bits cannot be guessed, so there is nothing to rate-limit for the sake of the code itself.
pub fn new_invite_code() -> Result<(lazy_protocol::InviteCode, [u8; 32]), AppError> {
    let mut random = [0u8; lazy_domain::workspace::INVITE_CODE_ENTROPY_BYTES];
    fill_random(&mut random)?;
    let code = lazy_domain::workspace::invite_code_from_entropy(&random);
    let hash = invite_code_hash(&code);
    Ok((lazy_protocol::InviteCode::new(code), hash))
}

/// The hash an invitation code is stored and looked up as. Any text hashes, so a guess of the wrong
/// shape costs the same as one of the right shape.
pub fn invite_code_hash(code: &str) -> [u8; 32] {
    Sha256::digest(code.as_bytes()).into()
}

/// The hash a webhook secret is stored and compared as. Any text can be hashed, so a guess of the
/// wrong shape costs the same as one of the right shape.
pub fn webhook_secret_hash(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

/// The value stored for a token. Returns `None` for text that cannot be a token we issued.
pub fn token_hash_if_wellformed(token: &str) -> Option<[u8; 32]> {
    let random = token.strip_prefix(TOKEN_PREFIX)?;
    let wellformed = random.len() == TOKEN_RANDOM_BYTES * 2
        && random
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    wellformed.then(|| token_hash(token))
}

fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Fingerprints of a send request, stored with its operation receipt to detect an operation id
/// reused for a different message.
///
/// The first is the one to store. A document is hashed in its serialized form, which carries the
/// document version, and documents moved from version 1 to 2; a receipt written before that holds
/// the version 1 form. The second is that form, so that a person whose acknowledgment was lost
/// across an upgrade, and who retries the same send, is recognized rather than told they conflict
/// with themselves. It can never match a document that uses anything version 1 lacked.
pub fn request_fingerprints(
    conversation_id: ConversationId,
    document: &lazy_domain::MessageDocument,
) -> Result<[[u8; 32]; 2], AppError> {
    let current = serde_json::to_vec(document)
        .map_err(|e| AppError::internal(format!("cannot serialize document: {e}")))?;
    let blocks = serde_json::to_string(document.blocks())
        .map_err(|e| AppError::internal(format!("cannot serialize document: {e}")))?;
    let legacy = format!("{{\"version\":1,\"blocks\":{blocks}}}").into_bytes();
    Ok([
        send_fingerprint(conversation_id, &current),
        send_fingerprint(conversation_id, &legacy),
    ])
}

fn send_fingerprint(conversation_id: ConversationId, document: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(conversation_id.as_uuid().as_bytes());
    hasher.update(document);
    hasher.finalize().into()
}

/// `fingerprints` of a send, with the files it names mixed in (in order), so that an operation id
/// reused for the same words with other files is a conflict. A send without files keeps the
/// fingerprint it always had.
pub fn attachment_fingerprints(
    fingerprints: [[u8; 32]; 2],
    attachments: &[lazy_domain::AttachmentId],
) -> [[u8; 32]; 2] {
    if attachments.is_empty() {
        return fingerprints;
    }
    fingerprints.map(|fingerprint| {
        let mut hasher = Sha256::new();
        hasher.update(b"attachments");
        hasher.update(fingerprint);
        for id in attachments {
            hasher.update(id.as_uuid().as_bytes());
        }
        hasher.finalize().into()
    })
}

/// [`request_fingerprints`] for a reply: the same, with the thread it is in and whether it was
/// also sent to the channel mixed in, so an operation id reused for the same words in another
/// thread (or without the flag) is a conflict. A send that is not a reply keeps the fingerprint it
/// always had.
pub fn reply_fingerprints(
    conversation_id: ConversationId,
    document: &lazy_domain::MessageDocument,
    root: lazy_domain::MessageId,
    also_in_channel: bool,
) -> Result<[[u8; 32]; 2], AppError> {
    let base = request_fingerprints(conversation_id, document)?;
    Ok(base.map(|fingerprint| {
        let mut hasher = Sha256::new();
        hasher.update(b"reply");
        hasher.update(fingerprint);
        hasher.update(root.as_uuid().as_bytes());
        hasher.update([u8::from(also_in_channel)]);
        hasher.finalize().into()
    }))
}

/// Fingerprint of a command other than a send, stored with its receipt. `tag` names the command
/// and each part is length-prefixed, so different commands or different splits of the same bytes
/// never collide.
pub fn command_fingerprint(tag: &str, parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(tag.as_bytes());
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn fill_random(buffer: &mut [u8]) -> Result<(), AppError> {
    getrandom::fill(buffer).map_err(|e| AppError::internal(format!("no system randomness: {e}")))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            // Writing to a String cannot fail.
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use lazy_domain::MessageDocument;

    use super::*;

    #[test]
    fn tokens_are_random_prefixed_and_hash_consistently() {
        let (a, a_hash) = new_session_token().unwrap();
        let (b, _) = new_session_token().unwrap();
        assert_ne!(a, b);
        assert!(a.expose().starts_with("lzy_"));
        assert_eq!(a.expose().len(), 4 + 64);
        assert_eq!(token_hash_if_wellformed(a.expose()), Some(a_hash));
    }

    #[test]
    fn malformed_tokens_are_rejected_before_hashing() {
        for bad in [
            "",
            "lzy_",
            "lzy_xyz",
            "abc",
            &format!("lzy_{}", "g".repeat(64)),
            &format!("lzy_{}", "A".repeat(64)),
        ] {
            assert_eq!(token_hash_if_wellformed(bad), None, "{bad}");
        }
    }

    #[test]
    fn password_policy_counts_bytes() {
        assert!(check_password_policy(&Password::new("1234567")).is_err());
        assert!(check_password_policy(&Password::new("12345678")).is_ok());
        assert!(check_password_policy(&Password::new("x".repeat(256))).is_ok());
        assert!(check_password_policy(&Password::new("x".repeat(257))).is_err());
    }

    #[tokio::test]
    async fn hashing_roundtrips_with_default_cost() {
        let service = PasswordService::new(PasswordCost::DEFAULT).unwrap();
        let password = Password::new("correct horse battery");
        let hash = service.hash(&password).await.unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(service.verify(&password, Some(&hash)).await.unwrap());
        assert!(
            !service
                .verify(&Password::new("wrong password"), Some(&hash))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn unknown_users_never_verify() {
        let service = PasswordService::new(PasswordCost::FAST_FOR_TESTS).unwrap();
        assert!(
            !service
                .verify(&Password::new("anything at all"), None)
                .await
                .unwrap()
        );
    }

    #[test]
    fn command_fingerprints_separate_commands_and_part_boundaries() {
        let base = command_fingerprint("create_channel", &[b"ab", b"c"]);
        assert_eq!(base, command_fingerprint("create_channel", &[b"ab", b"c"]));
        assert_ne!(
            base,
            command_fingerprint("open_conversation", &[b"ab", b"c"])
        );
        assert_ne!(base, command_fingerprint("create_channel", &[b"a", b"bc"]));
        assert_ne!(base, command_fingerprint("create_channel", &[b"abc"]));
    }

    #[test]
    fn fingerprints_depend_on_conversation_and_content() {
        let conversation = ConversationId::generate();
        let one = MessageDocument::from_plain_text("one").unwrap();
        let two = MessageDocument::from_plain_text("two").unwrap();
        let base = request_fingerprints(conversation, &one).unwrap();
        assert_eq!(base, request_fingerprints(conversation, &one).unwrap());
        assert_ne!(base, request_fingerprints(conversation, &two).unwrap());
        assert_ne!(
            base,
            request_fingerprints(ConversationId::generate(), &one).unwrap()
        );
        assert_ne!(
            base[0], base[1],
            "the legacy form differs from the current one"
        );
    }

    #[test]
    fn the_legacy_fingerprint_is_what_a_version_1_server_stored() {
        // A receipt written before documents moved to version 2 hashed exactly these bytes.
        let conversation = ConversationId::generate();
        let document = MessageDocument::from_plain_text("one").unwrap();
        let stored_then = br#"{"version":1,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"one"}]}]}"#;
        assert_eq!(
            request_fingerprints(conversation, &document).unwrap()[1],
            send_fingerprint(conversation, stored_then)
        );
    }
}
