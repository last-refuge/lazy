//! Room reports: what the SFU tells the host application about who is in its rooms.
//!
//! The SFU `POST`s a [`RoomReport`] as JSON to the host's address whenever someone joins or leaves
//! a room, and again every [`HEARTBEAT_SECS`] for every room with someone in it. A report is the
//! room's complete state, not a change, so a lost or repeated one does no lasting harm: the next
//! one corrects it. A room the host has not heard about for a few heartbeats has ended (the SFU
//! may have restarted). An empty `participants` list says the room just emptied.
//!
//! The body is signed with the secret the host and the SFU share (the one join tokens use): the
//! [`SIGNATURE_HEADER`] holds `v1=` and the base64url HMAC-SHA256 of `rtc-report.` followed by the
//! body. The prefix keeps a report's signature from ever being a token's. `sent_at_ms` lets the host
//! refuse an old report played back to it ([`verify`] allows [`MAX_AGE_SECS`]) and keep only the
//! newest report per room when two arrive out of order.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::token::MIN_SECRET_BYTES;

/// The header carrying a report's signature.
pub const SIGNATURE_HEADER: &str = "rtc-signature";

/// How often the SFU repeats the state of every room with someone in it.
pub const HEARTBEAT_SECS: u64 = 30;

/// How far `sent_at_ms` may be from the receiver's clock, either way.
pub const MAX_AGE_SECS: u64 = 300;

const PREFIX: &[u8] = b"rtc-report.";
const VERSION: &str = "v1=";

/// Who is in one room, in the order they joined.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomReport {
    pub room: String,
    pub participants: Vec<ReportedParticipant>,
    /// When the SFU made it, in milliseconds since the Unix epoch.
    pub sent_at_ms: u64,
}

/// Someone in a room, as their join token named them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedParticipant {
    pub identity: String,
    pub name: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReportError {
    #[error("the report's signature is missing or malformed")]
    Malformed,
    #[error("the report's signature does not match")]
    BadSignature,
    #[error("the report is too old, or from the future")]
    Stale,
    #[error("the secret is too short (at least {MIN_SECRET_BYTES} bytes)")]
    WeakSecret,
}

fn mac(secret: &[u8], body: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC takes any key length");
    mac.update(PREFIX);
    mac.update(body);
    mac
}

/// The [`SIGNATURE_HEADER`] value for `body`.
pub fn sign(body: &[u8], secret: &[u8]) -> Result<String, ReportError> {
    if secret.len() < MIN_SECRET_BYTES {
        return Err(ReportError::WeakSecret);
    }
    let signature = URL_SAFE_NO_PAD.encode(mac(secret, body).finalize().into_bytes());
    Ok(format!("{VERSION}{signature}"))
}

/// Checks `signature` (the header's value) over `body` and the report's age against `now_ms`
/// (milliseconds since the Unix epoch), and returns the report.
pub fn verify(
    body: &[u8],
    signature: &str,
    secret: &[u8],
    now_ms: u64,
) -> Result<RoomReport, ReportError> {
    if secret.len() < MIN_SECRET_BYTES {
        return Err(ReportError::WeakSecret);
    }
    let signature = signature
        .trim()
        .strip_prefix(VERSION)
        .and_then(|s| URL_SAFE_NO_PAD.decode(s).ok())
        .ok_or(ReportError::Malformed)?;
    mac(secret, body)
        .verify_slice(&signature)
        .map_err(|_| ReportError::BadSignature)?;
    let report: RoomReport = serde_json::from_slice(body).map_err(|_| ReportError::Malformed)?;
    if report.sent_at_ms.abs_diff(now_ms) > MAX_AGE_SECS * 1000 {
        return Err(ReportError::Stale);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn body() -> Vec<u8> {
        serde_json::to_vec(&RoomReport {
            room: "room-1".into(),
            participants: vec![ReportedParticipant {
                identity: "user-1".into(),
                name: "Ada".into(),
            }],
            sent_at_ms: 1_000_000,
        })
        .unwrap()
    }

    #[test]
    fn a_signed_report_verifies_while_it_is_fresh() {
        let body = body();
        let signature = sign(&body, SECRET).unwrap();
        assert!(signature.starts_with("v1="));
        let report = verify(&body, &signature, SECRET, 1_000_000 + MAX_AGE_SECS * 1000).unwrap();
        assert_eq!(report.participants[0].name, "Ada");
        assert_eq!(
            verify(&body, &signature, SECRET, 1_000_001 + MAX_AGE_SECS * 1000),
            Err(ReportError::Stale)
        );
        assert_eq!(
            verify(
                &body,
                &signature,
                SECRET,
                1_000_000 - MAX_AGE_SECS * 1000 - 1
            ),
            Err(ReportError::Stale)
        );
    }

    #[test]
    fn a_forged_or_altered_report_is_refused() {
        let body = body();
        let signature = sign(&body, SECRET).unwrap();
        let other = b"fedcba9876543210fedcba9876543210";
        assert_eq!(
            verify(&body, &signature, other, 1_000_000),
            Err(ReportError::BadSignature)
        );
        let mut altered = body.clone();
        let at = altered.iter().position(|b| *b == b'A').unwrap();
        altered[at] = b'B';
        assert_eq!(
            verify(&altered, &signature, SECRET, 1_000_000),
            Err(ReportError::BadSignature)
        );
        for junk in ["", "v1=", "v2=abc", "v1=!!"] {
            assert!(verify(&body, junk, SECRET, 1_000_000).is_err(), "{junk}");
        }
        // A join token's signature over the same bytes is not a report's.
        assert_ne!(sign(&body, SECRET).unwrap(), {
            let mut mac = Hmac::<Sha256>::new_from_slice(SECRET).unwrap();
            mac.update(&body);
            format!("v1={}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
        });
    }

    #[test]
    fn short_secrets_are_refused() {
        assert_eq!(sign(b"{}", b"short"), Err(ReportError::WeakSecret));
        assert_eq!(
            verify(b"{}", "v1=a", b"short", 0),
            Err(ReportError::WeakSecret)
        );
    }
}
