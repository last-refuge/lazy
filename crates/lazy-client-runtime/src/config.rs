//! Runtime configuration.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::secure::SecureStorage;

/// Server used when neither `LAZY_SERVER_URL` nor the person has chosen one.
pub const DEFAULT_SERVER_URL: &str = lazy_client_core::DEFAULT_SERVER_URL;

/// The server base URL from `LAZY_SERVER_URL`, or [`DEFAULT_SERVER_URL`].
pub fn default_server_url() -> String {
    environment_server_url().unwrap_or_else(|| DEFAULT_SERVER_URL.to_owned())
}

/// `LAZY_SERVER_URL`, if it is set to something.
fn environment_server_url() -> Option<String> {
    std::env::var("LAZY_SERVER_URL")
        .ok()
        .map(|url| url.trim().to_owned())
        .filter(|url| !url.is_empty())
}

/// Where the session token and the file-encryption key are kept.
#[derive(Clone)]
pub enum SecureStorageChoice {
    /// The operating system's credential store (Secret Service on Linux, Keychain on macOS,
    /// Credential Manager on Windows). If it is unavailable the runtime reports the problem and
    /// refuses to persist; it never falls back to anything weaker on its own.
    Keyring,
    /// Plain files inside the profile directory. **Not secure**: anyone who can read the profile
    /// can read the session token and the key that protects the local state. For development,
    /// CI, and containers without a credential store; it must be chosen explicitly.
    InsecureFile,
    /// A caller-provided store, for tests and embedding.
    Custom(Arc<dyn SecureStorage>),
}

impl std::fmt::Debug for SecureStorageChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keyring => f.write_str("Keyring"),
            Self::InsecureFile => f.write_str("InsecureFile"),
            Self::Custom(_) => f.write_str("Custom"),
        }
    }
}

/// Exponential backoff with jitter, capped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackoffPolicy {
    pub base: Duration,
    pub cap: Duration,
}

impl BackoffPolicy {
    /// Delay before retry number `attempt` (1 for the first retry). Never exceeds `cap`.
    /// `entropy` selects the jitter within the upper half of the nominal delay.
    pub fn delay(&self, attempt: u32, entropy: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(20);
        let nominal = self.base.saturating_mul(1u32 << exponent).min(self.cap);
        let half = nominal / 2;
        let jitter = half.mul_f64(f64::from(entropy) / f64::from(u32::MAX));
        (half + jitter).min(self.cap)
    }
}

/// Settings for [`crate::Runtime::start`].
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Base URL of the Lazy server. Defaults from `LAZY_SERVER_URL`.
    pub server_url: String,
    /// `server_url` was set explicitly (by the environment, or by whoever started the runtime):
    /// it wins over a server the person chose in the application, and the application cannot
    /// change it. When `false`, a saved choice (`server.json` in the profile) wins over
    /// `server_url`, which is then only the default.
    pub server_locked: bool,
    /// Directory for encrypted local state. `None` uses `LAZY_PROFILE_DIR`, then the platform's
    /// per-user data directory. Use a distinct directory per account or test client.
    pub profile_dir: Option<PathBuf>,
    pub secure_storage: SecureStorageChoice,
    /// Per-request timeout. A timeout never proves a mutation failed.
    pub request_timeout: Duration,
    /// How many times a safe-to-repeat read (bootstrap, history, status) is attempted.
    pub read_attempts: u32,
    /// Reconnect schedule for realtime connections.
    pub reconnect: BackoffPolicy,
    /// A realtime connection with no traffic for this long is treated as dead.
    pub idle_timeout: Duration,
    /// How long draft and cursor writes are coalesced.
    pub save_debounce: Duration,
}

impl RuntimeConfig {
    pub fn new(server_url: impl Into<String>) -> Self {
        Self {
            server_url: server_url.into(),
            server_locked: true,
            ..Self::default()
        }
    }

    /// Defaults, honoring `LAZY_SERVER_URL`, `LAZY_PROFILE_DIR` (read at start), and
    /// `LAZY_INSECURE_STORAGE=1` (explicit opt-in to [`SecureStorageChoice::InsecureFile`]).
    pub fn from_env() -> Self {
        let insecure = std::env::var("LAZY_INSECURE_STORAGE").is_ok_and(|v| v == "1");
        let environment = environment_server_url();
        Self {
            server_locked: environment.is_some(),
            server_url: environment.unwrap_or_else(|| DEFAULT_SERVER_URL.to_owned()),
            profile_dir: None,
            secure_storage: if insecure {
                SecureStorageChoice::InsecureFile
            } else {
                SecureStorageChoice::Keyring
            },
            request_timeout: Duration::from_secs(15),
            read_attempts: 4,
            reconnect: BackoffPolicy {
                base: Duration::from_millis(500),
                cap: Duration::from_secs(15),
            },
            idle_timeout: Duration::from_secs(90),
            save_debounce: Duration::from_millis(400),
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_never_exceeds_the_cap() {
        let policy = BackoffPolicy {
            base: Duration::from_millis(500),
            cap: Duration::from_secs(15),
        };
        let mut previous_max = Duration::ZERO;
        for attempt in 1..=200 {
            for entropy in [0, u32::MAX / 2, u32::MAX] {
                assert!(policy.delay(attempt, entropy) <= policy.cap);
            }
            let max = policy.delay(attempt, u32::MAX);
            assert!(max >= previous_max, "the upper bound never shrinks");
            previous_max = max;
        }
        assert_eq!(policy.delay(200, u32::MAX), policy.cap);
        assert!(policy.delay(1, 0) >= Duration::from_millis(250));
    }

    #[test]
    fn default_server_url_falls_back_when_unset() {
        // Reading the environment is process-global, so only the fallback shape is asserted.
        let url = default_server_url();
        assert!(url.starts_with("http"));
    }
}
