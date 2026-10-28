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

/// Fingerprint of a send request, stored with its operation receipt to detect an operation id
/// reused for a different message.
pub fn request_fingerprint(
    conversation_id: ConversationId,
    document: &lazy_domain::MessageDocument,
) -> Result<[u8; 32], AppError> {
    let body = serde_json::to_vec(document)
        .map_err(|e| AppError::internal(format!("cannot serialize document: {e}")))?;
    let mut hasher = Sha256::new();
    hasher.update(conversation_id.as_uuid().as_bytes());
    hasher.update(&body);
    Ok(hasher.finalize().into())
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
    fn fingerprints_depend_on_conversation_and_content() {
        let conversation = ConversationId::generate();
        let one = MessageDocument::from_plain_text("one").unwrap();
        let two = MessageDocument::from_plain_text("two").unwrap();
        let base = request_fingerprint(conversation, &one).unwrap();
        assert_eq!(base, request_fingerprint(conversation, &one).unwrap());
        assert_ne!(base, request_fingerprint(conversation, &two).unwrap());
        assert_ne!(
            base,
            request_fingerprint(ConversationId::generate(), &one).unwrap()
        );
    }
}
