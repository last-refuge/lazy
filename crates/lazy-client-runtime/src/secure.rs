//! Secure storage for the session token and the key that protects local state.
//!
//! Entries are namespaced by profile directory and server URL, so two clients on one machine (or
//! one client signed in to two servers) never read, overwrite, or delete each other's secrets.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lazy_protocol::SessionToken;
use sha2::{Digest, Sha256};

/// Service name used for credential store entries.
const SERVICE: &str = "app.lazy.client";
const SESSION_TOKEN: &str = "session-token";
const STATE_KEY: &str = "state-key";

/// Why a secret could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("secure storage is unavailable: {0}")]
    Unavailable(String),
    #[error("secure storage failed: {0}")]
    Failed(String),
    #[error("stored secret is malformed")]
    Malformed,
}

/// A place to keep small secrets.
///
/// Implementations are blocking; the runtime calls them from blocking threads. `delete` succeeds
/// when the entry does not exist.
pub trait SecureStorage: Send + Sync + 'static {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError>;
    fn set(&self, key: &str, value: &[u8]) -> Result<(), StorageError>;
    fn delete(&self, key: &str) -> Result<(), StorageError>;
}

/// The operating system's credential store.
#[derive(Debug)]
pub struct KeyringStorage;

impl KeyringStorage {
    /// Fails if the platform credential store cannot be initialized.
    pub fn new() -> Result<Self, StorageError> {
        match keyring::Entry::store_status() {
            Ok(()) => Ok(Self),
            Err(error) => Err(StorageError::Unavailable(error.to_string())),
        }
    }

    fn entry(key: &str) -> Result<keyring::Entry, StorageError> {
        keyring::Entry::new(SERVICE, key).map_err(|e| StorageError::Failed(e.to_string()))
    }
}

impl SecureStorage for KeyringStorage {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        match Self::entry(key)?.get_secret() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(StorageError::Failed(error.to_string())),
        }
    }

    fn set(&self, key: &str, value: &[u8]) -> Result<(), StorageError> {
        Self::entry(key)?
            .set_secret(value)
            .map_err(|e| StorageError::Failed(e.to_string()))
    }

    fn delete(&self, key: &str) -> Result<(), StorageError> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(StorageError::Failed(error.to_string())),
        }
    }
}

/// In-memory storage for tests.
#[derive(Debug, Default)]
pub struct MemoryStorage {
    entries: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemoryStorage {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Names of all stored entries, sorted.
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.lock().keys().cloned().collect();
        keys.sort();
        keys
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<u8>>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl SecureStorage for MemoryStorage {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(self.lock().get(key).cloned())
    }

    fn set(&self, key: &str, value: &[u8]) -> Result<(), StorageError> {
        self.lock().insert(key.to_owned(), value.to_vec());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.lock().remove(key);
        Ok(())
    }
}

/// Secrets as plain files with owner-only permissions.
///
/// **Not secure.** It exists for development, CI, and containers that have no credential store,
/// and is used only when [`crate::SecureStorageChoice::InsecureFile`] is chosen explicitly.
#[derive(Debug)]
pub struct InsecureFileStorage {
    dir: PathBuf,
}

impl InsecureFileStorage {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, key: &str) -> PathBuf {
        let name: String = key
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(name)
    }
}

impl SecureStorage for InsecureFileStorage {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        match std::fs::read(self.path(key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(StorageError::Failed(error.to_string())),
        }
    }

    fn set(&self, key: &str, value: &[u8]) -> Result<(), StorageError> {
        let fail = |e: std::io::Error| StorageError::Failed(e.to_string());
        std::fs::create_dir_all(&self.dir).map_err(fail)?;
        crate::files::write_atomic(&self.path(key), value, &mut |_| Ok(())).map_err(fail)
    }

    fn delete(&self, key: &str) -> Result<(), StorageError> {
        match std::fs::remove_file(self.path(key)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(StorageError::Failed(error.to_string())),
        }
    }
}

/// A stable identifier for one (profile directory, server) pair, used to namespace secrets.
///
/// `profile_dir` should be canonical. The server URL is compared without a trailing slash and
/// case-insensitively.
pub fn secret_namespace(profile_dir: &Path, server_url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"lazy-secret-namespace-v1\0");
    hasher.update(profile_dir.as_os_str().as_encoded_bytes());
    hasher.update(b"\0");
    hasher.update(
        server_url
            .trim()
            .trim_end_matches('/')
            .to_ascii_lowercase()
            .as_bytes(),
    );
    let digest = hasher.finalize();
    digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The runtime's secrets for one profile and server.
#[derive(Clone)]
pub struct SecretVault {
    storage: Arc<dyn SecureStorage>,
    namespace: String,
}

impl SecretVault {
    pub fn new(storage: Arc<dyn SecureStorage>, profile_dir: &Path, server_url: &str) -> Self {
        let canonical =
            std::fs::canonicalize(profile_dir).unwrap_or_else(|_| profile_dir.to_owned());
        Self {
            storage,
            namespace: secret_namespace(&canonical, server_url),
        }
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    fn key(&self, kind: &str) -> String {
        format!("{kind}:{}", self.namespace)
    }

    pub fn session_token(&self) -> Result<Option<SessionToken>, StorageError> {
        match self.storage.get(&self.key(SESSION_TOKEN))? {
            None => Ok(None),
            Some(bytes) => String::from_utf8(bytes)
                .map(|token| Some(SessionToken::new(token)))
                .map_err(|_| StorageError::Malformed),
        }
    }

    pub fn set_session_token(&self, token: &SessionToken) -> Result<(), StorageError> {
        self.storage
            .set(&self.key(SESSION_TOKEN), token.expose().as_bytes())
    }

    pub fn delete_session_token(&self) -> Result<(), StorageError> {
        self.storage.delete(&self.key(SESSION_TOKEN))
    }

    /// The 256-bit key protecting local state files, created on first use.
    pub fn state_key(&self) -> Result<[u8; 32], StorageError> {
        let key_name = self.key(STATE_KEY);
        if let Some(bytes) = self.storage.get(&key_name)? {
            return <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| StorageError::Malformed);
        }
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).map_err(|e| StorageError::Failed(e.to_string()))?;
        self.storage.set(&key_name, &key)?;
        Ok(key)
    }

    /// Deletes every secret of this profile and server, and nothing else.
    pub fn delete_all(&self) -> Result<(), StorageError> {
        let session = self.storage.delete(&self.key(SESSION_TOKEN));
        let key = self.storage.delete(&self.key(STATE_KEY));
        session.and(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault(storage: &Arc<MemoryStorage>, dir: &Path, server: &str) -> SecretVault {
        SecretVault::new(storage.clone(), dir, server)
    }

    #[test]
    fn two_profiles_on_one_keyring_never_share_or_delete_each_others_secrets() {
        let shared = MemoryStorage::new();
        let a_dir = tempfile::tempdir().unwrap();
        let b_dir = tempfile::tempdir().unwrap();
        let a = vault(&shared, a_dir.path(), "http://127.0.0.1:8080");
        let b = vault(&shared, b_dir.path(), "http://127.0.0.1:8080");
        assert_ne!(a.namespace(), b.namespace());

        a.set_session_token(&SessionToken::new("token-a")).unwrap();
        b.set_session_token(&SessionToken::new("token-b")).unwrap();
        let key_a = a.state_key().unwrap();
        let key_b = b.state_key().unwrap();
        assert_ne!(key_a, key_b, "each profile has its own encryption key");
        assert_eq!(a.session_token().unwrap().unwrap().expose(), "token-a");
        assert_eq!(b.session_token().unwrap().unwrap().expose(), "token-b");

        // Signing out of A removes only A's secrets.
        a.delete_all().unwrap();
        assert!(a.session_token().unwrap().is_none());
        assert_eq!(b.session_token().unwrap().unwrap().expose(), "token-b");
        assert_eq!(b.state_key().unwrap(), key_b);
        assert_eq!(
            shared.keys().len(),
            2,
            "only B's two entries remain: {:?}",
            shared.keys()
        );
    }

    #[test]
    fn one_profile_signed_in_to_two_servers_keeps_separate_secrets() {
        let shared = MemoryStorage::new();
        let dir = tempfile::tempdir().unwrap();
        let local = vault(&shared, dir.path(), "http://127.0.0.1:8080");
        let remote = vault(&shared, dir.path(), "https://chat.example.com");
        local
            .set_session_token(&SessionToken::new("local"))
            .unwrap();
        assert!(
            remote.session_token().unwrap().is_none(),
            "a token is never offered to another server"
        );
    }

    #[test]
    fn the_same_profile_and_server_always_find_the_same_secrets() {
        let shared = MemoryStorage::new();
        let dir = tempfile::tempdir().unwrap();
        let first = vault(&shared, dir.path(), "http://localhost:8080/");
        first.set_session_token(&SessionToken::new("t")).unwrap();
        let key = first.state_key().unwrap();

        // A trailing slash and letter case in the URL do not matter; neither does the way the
        // directory is spelled.
        let dotted = dir.path().join("sub").join("..");
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let second = vault(&shared, &dotted, "HTTP://LOCALHOST:8080");
        assert_eq!(first.namespace(), second.namespace());
        assert_eq!(second.session_token().unwrap().unwrap().expose(), "t");
        assert_eq!(second.state_key().unwrap(), key);
    }

    /// Exercises the real credential store (the Secret Service on Linux). Opt-in because it
    /// touches the user's keyring: it writes one uniquely named entry and deletes it again.
    /// Run with `cargo test -p lazy-client-runtime -- --ignored keyring`.
    #[test]
    #[ignore = "uses the machine's real credential store"]
    fn keyring_round_trip_on_this_machine() {
        let storage = KeyringStorage::new().expect("a credential store is available");
        let mut unique = [0u8; 8];
        getrandom::fill(&mut unique).unwrap();
        let name = format!(
            "lazy-test-{}",
            unique
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        assert_eq!(storage.get(&name).unwrap(), None);
        storage.set(&name, b"\x00binary\xffsecret").unwrap();
        assert_eq!(
            storage.get(&name).unwrap().as_deref(),
            Some(&b"\x00binary\xffsecret"[..])
        );
        storage.delete(&name).unwrap();
        storage.delete(&name).unwrap();
        assert_eq!(storage.get(&name).unwrap(), None);
    }

    #[test]
    fn insecure_file_storage_round_trips_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let storage = InsecureFileStorage::new(dir.path().join("secrets"));
        assert_eq!(storage.get("session-token:abc").unwrap(), None);
        storage.set("session-token:abc", b"secret").unwrap();
        assert_eq!(
            storage.get("session-token:abc").unwrap().as_deref(),
            Some(&b"secret"[..])
        );
        storage.delete("session-token:abc").unwrap();
        storage.delete("session-token:abc").unwrap();
        assert_eq!(storage.get("session-token:abc").unwrap(), None);
    }
}
