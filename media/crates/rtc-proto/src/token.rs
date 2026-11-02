//! Join tokens: `rtc1.<claims>.<signature>`, both parts base64url without padding. The claims are
//! JSON ([`Claims`]); the signature is HMAC-SHA256 over `rtc1.<claims>` with a secret the host
//! application and the SFU share. A token is checked for its signature and expiry, nothing else:
//! whoever holds a valid one may join its room as its identity until it expires, so hosts issue
//! them for minutes, not days.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

const PREFIX: &str = "rtc1.";

/// A secret shorter than this is refused: it could be guessed.
pub const MIN_SECRET_BYTES: usize = 32;

/// What a token allows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// The room to join; rooms exist while someone is in them.
    pub room: String,
    /// The host application's identifier for the person.
    pub identity: String,
    /// The name others see.
    pub name: String,
    /// Expiry, in seconds since the Unix epoch.
    pub exp: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TokenError {
    #[error("the token is malformed")]
    Malformed,
    #[error("the token's signature does not match")]
    BadSignature,
    #[error("the token has expired")]
    Expired,
    #[error("the secret is too short (at least {MIN_SECRET_BYTES} bytes)")]
    WeakSecret,
}

fn mac(secret: &[u8], signed: &str) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC takes any key length");
    mac.update(signed.as_bytes());
    mac
}

/// Signs `claims` with `secret`.
pub fn sign(claims: &Claims, secret: &[u8]) -> Result<String, TokenError> {
    if secret.len() < MIN_SECRET_BYTES {
        return Err(TokenError::WeakSecret);
    }
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("claims serialize"));
    let signed = format!("{PREFIX}{body}");
    let signature = URL_SAFE_NO_PAD.encode(mac(secret, &signed).finalize().into_bytes());
    Ok(format!("{signed}.{signature}"))
}

/// Checks `token`'s signature with `secret` and its expiry against `now` (seconds since the Unix
/// epoch), and returns its claims.
pub fn verify(token: &str, secret: &[u8], now: u64) -> Result<Claims, TokenError> {
    if secret.len() < MIN_SECRET_BYTES {
        return Err(TokenError::WeakSecret);
    }
    let rest = token.strip_prefix(PREFIX).ok_or(TokenError::Malformed)?;
    let (body, signature) = rest.split_once('.').ok_or(TokenError::Malformed)?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| TokenError::Malformed)?;
    mac(secret, &token[..PREFIX.len() + body.len()])
        .verify_slice(&signature)
        .map_err(|_| TokenError::BadSignature)?;
    let claims: Claims = URL_SAFE_NO_PAD
        .decode(body)
        .ok()
        .and_then(|json| serde_json::from_slice(&json).ok())
        .ok_or(TokenError::Malformed)?;
    if claims.exp <= now {
        return Err(TokenError::Expired);
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn claims() -> Claims {
        Claims {
            room: "room-1".into(),
            identity: "user-1".into(),
            name: "Ada".into(),
            exp: 1_000,
        }
    }

    #[test]
    fn a_signed_token_verifies_until_it_expires() {
        let token = sign(&claims(), SECRET).unwrap();
        assert!(token.starts_with("rtc1."));
        assert_eq!(verify(&token, SECRET, 999).unwrap(), claims());
        assert_eq!(verify(&token, SECRET, 1_000), Err(TokenError::Expired));
    }

    #[test]
    fn a_forged_or_altered_token_is_refused() {
        let token = sign(&claims(), SECRET).unwrap();
        let other = b"fedcba9876543210fedcba9876543210";
        assert_eq!(verify(&token, other, 0), Err(TokenError::BadSignature));

        let mut forged = claims();
        forged.room = "someone-elses".into();
        let forged_body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged).unwrap());
        let signature = token.rsplit_once('.').unwrap().1;
        let altered = format!("rtc1.{forged_body}.{signature}");
        assert_eq!(verify(&altered, SECRET, 0), Err(TokenError::BadSignature));

        for junk in ["", "rtc1.", "rtc1.abc", "rtc2.a.b", "rtc1.a.!!"] {
            assert_eq!(
                verify(junk, SECRET, 0),
                Err(TokenError::Malformed),
                "{junk}"
            );
        }
    }

    #[test]
    fn short_secrets_are_refused() {
        assert_eq!(sign(&claims(), b"short"), Err(TokenError::WeakSecret));
        assert_eq!(verify("rtc1.a.b", b"short", 0), Err(TokenError::WeakSecret));
    }
}
