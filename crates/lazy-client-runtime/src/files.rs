//! Encrypted, atomically replaced state files.
//!
//! Layout of a file: `LZST`, format version (u16 LE), kind (u8), a 24-byte random nonce, then the
//! XChaCha20-Poly1305 ciphertext of a JSON payload. The first seven bytes are authenticated, so a
//! file cannot be passed off as a different kind or version.
//!
//! Writes go to a temporary file in the same directory, are flushed to disk, then renamed over the
//! target and the directory is flushed. A crash at any point leaves either the previous version
//! or the new one. Loading never silently discards data: a file that cannot be read is renamed
//! aside and reported, and a file written by a newer version is left untouched.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, Generate, Key, KeyInit, Nonce, Payload};
use serde::Serialize;
use serde::de::DeserializeOwned;

const MAGIC: &[u8; 4] = b"LZST";
/// The file format this build reads and writes.
pub(crate) const FORMAT: u16 = 1;
const HEADER_LEN: usize = 7;
const NONCE_LEN: usize = 24;
/// Quarantined files kept per state file.
const MAX_QUARANTINED: usize = 3;

/// Which state a file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum FileKind {
    Journal,
    Drafts,
    /// The regenerable content cache (see `lazy_client_core::ContentCache`).
    Cache,
}

impl FileKind {
    pub(crate) const ALL: [FileKind; 3] = [Self::Journal, Self::Drafts, Self::Cache];

    fn id(self) -> u8 {
        match self {
            Self::Journal => 1,
            Self::Drafts => 2,
            Self::Cache => 4,
        }
    }

    pub(crate) fn file_name(self) -> &'static str {
        match self {
            Self::Journal => "journal.enc",
            Self::Drafts => "drafts.enc",
            Self::Cache => "cache.enc",
        }
    }

    /// Largest file accepted when loading or produced when saving.
    pub(crate) fn max_bytes(self) -> u64 {
        match self {
            Self::Journal => 16 * 1024 * 1024,
            Self::Drafts => 8 * 1024 * 1024,
            Self::Cache => lazy_client_core::MAX_CACHE_BYTES,
        }
    }

    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::Journal => "unsent-message journal",
            Self::Drafts => "drafts",
            Self::Cache => "content cache",
        }
    }
}

/// Steps of an atomic write, reported to a hook so tests can simulate a crash after any of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteStep {
    TempWritten,
    TempSynced,
    Renamed,
    DirSynced,
}

/// Writes `bytes` to `path` atomically. `hook` runs after each step; returning an error aborts
/// the write there, which tests use to model a crash.
pub(crate) fn write_atomic(
    path: &Path,
    bytes: &[u8],
    hook: &mut dyn FnMut(WriteStep) -> io::Result<()>,
) -> io::Result<()> {
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let mut temp_name = file_name.to_owned();
    temp_name.push(".tmp");
    let temp = path.with_file_name(temp_name);

    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temp)?;
    file.write_all(bytes)?;
    hook(WriteStep::TempWritten)?;
    file.sync_all()?;
    hook(WriteStep::TempSynced)?;
    drop(file);
    fs::rename(&temp, path)?;
    hook(WriteStep::Renamed)?;
    if let Some(dir) = path.parent() {
        sync_dir(dir)?;
    }
    hook(WriteStep::DirSynced)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Why a file could not be saved.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FileError {
    #[error("could not write {what}: {source}")]
    Io {
        what: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("{what} would exceed its size limit")]
    TooLarge { what: &'static str },
    #[error("could not encode {what}: {message}")]
    Encode { what: &'static str, message: String },
}

/// Result of loading one file.
#[derive(Debug)]
pub(crate) enum Loaded<T> {
    Missing,
    Value(T),
    /// Unreadable; it was moved aside (when possible) and the description says what happened.
    Corrupt(String),
    /// Written by a newer version. Not modified, and must not be overwritten.
    NewerFormat(u16),
    /// Could not be read for an I/O reason. Not modified, and must not be overwritten.
    Unreadable(String),
}

pub(crate) struct FileStore {
    dir: PathBuf,
    cipher: XChaCha20Poly1305,
}

enum OpenError {
    Truncated,
    BadMagic,
    WrongKind,
    Newer(u16),
    Authentication,
}

impl FileStore {
    pub(crate) fn new(dir: PathBuf, key: &[u8; 32]) -> Self {
        let key = Key::<XChaCha20Poly1305>::try_from(&key[..]).expect("a 32-byte key");
        Self {
            dir,
            cipher: XChaCha20Poly1305::new(&key),
        }
    }

    pub(crate) fn path(&self, kind: FileKind) -> PathBuf {
        self.dir.join(kind.file_name())
    }

    fn header(kind: FileKind) -> [u8; HEADER_LEN] {
        let mut header = [0u8; HEADER_LEN];
        header[..4].copy_from_slice(MAGIC);
        header[4..6].copy_from_slice(&FORMAT.to_le_bytes());
        header[6] = kind.id();
        header
    }

    fn seal(&self, kind: FileKind, plaintext: &[u8]) -> Result<Vec<u8>, ()> {
        let header = Self::header(kind);
        let nonce = Nonce::<XChaCha20Poly1305>::generate();
        let ciphertext = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: &header,
                },
            )
            .map_err(|_| ())?;
        let mut out = Vec::with_capacity(HEADER_LEN + NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&header);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    fn open(&self, kind: FileKind, bytes: &[u8]) -> Result<Vec<u8>, OpenError> {
        if bytes.len() < HEADER_LEN + NONCE_LEN {
            return Err(OpenError::Truncated);
        }
        if &bytes[..4] != MAGIC {
            return Err(OpenError::BadMagic);
        }
        let format = u16::from_le_bytes([bytes[4], bytes[5]]);
        if format > FORMAT {
            return Err(OpenError::Newer(format));
        }
        if format != FORMAT || bytes[6] != kind.id() {
            return Err(OpenError::WrongKind);
        }
        let nonce =
            Nonce::<XChaCha20Poly1305>::try_from(&bytes[HEADER_LEN..HEADER_LEN + NONCE_LEN])
                .map_err(|_| OpenError::Truncated)?;
        self.cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: &bytes[HEADER_LEN + NONCE_LEN..],
                    aad: &bytes[..HEADER_LEN],
                },
            )
            .map_err(|_| OpenError::Authentication)
    }

    pub(crate) fn load<T: DeserializeOwned>(&self, kind: FileKind) -> Loaded<T> {
        let path = self.path(kind);
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Loaded::Missing,
            Err(error) => return Loaded::Unreadable(error.to_string()),
        };
        if metadata.len() > kind.max_bytes() {
            return self.quarantine(kind, "it is larger than the allowed size");
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => return Loaded::Unreadable(error.to_string()),
        };
        let plaintext = match self.open(kind, &bytes) {
            Ok(plaintext) => plaintext,
            Err(OpenError::Newer(found)) => return Loaded::NewerFormat(found),
            Err(OpenError::Truncated) => return self.quarantine(kind, "it is truncated"),
            Err(OpenError::BadMagic) => {
                return self.quarantine(kind, "it is not a Lazy state file");
            }
            Err(OpenError::WrongKind) => {
                return self.quarantine(kind, "it holds the wrong kind of data");
            }
            Err(OpenError::Authentication) => {
                return self.quarantine(
                    kind,
                    "it could not be decrypted (damaged, or the key changed)",
                );
            }
        };
        match serde_json::from_slice(&plaintext) {
            Ok(value) => Loaded::Value(value),
            Err(error) => self.quarantine(
                kind,
                &format!(
                    "its contents are invalid ({})",
                    crate::api::describe_json(&error)
                ),
            ),
        }
    }

    /// Moves an unreadable file aside so it can be inspected, and describes what happened.
    fn quarantine<T>(&self, kind: FileKind, why: &str) -> Loaded<T> {
        let path = self.path(kind);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut target = self
            .dir
            .join(format!("{}.corrupt-{stamp}", kind.file_name()));
        let mut counter = 1;
        while target.exists() {
            target = self
                .dir
                .join(format!("{}.corrupt-{stamp}-{counter}", kind.file_name()));
            counter += 1;
        }
        let moved = fs::rename(&path, &target);
        self.prune_quarantine(kind);
        match moved {
            Ok(()) => Loaded::Corrupt(format!(
                "The {} was unreadable ({why}) and was set aside as {}.",
                kind.describe(),
                target
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
            )),
            Err(error) => Loaded::Unreadable(format!(
                "The {} is unreadable ({why}) and could not be set aside: {error}",
                kind.describe()
            )),
        }
    }

    fn prune_quarantine(&self, kind: FileKind) {
        let prefix = format!("{}.corrupt-", kind.file_name());
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut quarantined: Vec<_> = entries
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .filter_map(|e| e.metadata().ok()?.modified().ok().map(|m| (m, e.path())))
            .collect();
        quarantined.sort();
        let excess = quarantined.len().saturating_sub(MAX_QUARANTINED);
        for (_, path) in quarantined.into_iter().take(excess) {
            let _ = fs::remove_file(path);
        }
    }

    pub(crate) fn save<T: Serialize>(&self, kind: FileKind, value: &T) -> Result<(), FileError> {
        self.save_with_hook(kind, value, &mut |_| Ok(()))
    }

    pub(crate) fn save_with_hook<T: Serialize>(
        &self,
        kind: FileKind,
        value: &T,
        hook: &mut dyn FnMut(WriteStep) -> io::Result<()>,
    ) -> Result<(), FileError> {
        let what = kind.describe();
        let plaintext = serde_json::to_vec(value).map_err(|e| FileError::Encode {
            what,
            message: e.to_string(),
        })?;
        let sealed = self
            .seal(kind, &plaintext)
            .map_err(|()| FileError::Encode {
                what,
                message: "encryption failed".into(),
            })?;
        if sealed.len() as u64 > kind.max_bytes() {
            return Err(FileError::TooLarge { what });
        }
        write_atomic(&self.path(kind), &sealed, hook)
            .map_err(|source| FileError::Io { what, source })
    }

    /// Deletes a state file and its temporary file, keeping any quarantined copies.
    pub(crate) fn remove_current(&self, kind: FileKind) {
        let _ = fs::remove_file(self.path(kind));
        let _ = fs::remove_file(self.dir.join(format!("{}.tmp", kind.file_name())));
    }

    /// Deletes a file an earlier version used to write, if it is still around.
    pub(crate) fn remove_legacy(&self, name: &str) {
        let _ = fs::remove_file(self.dir.join(name));
        let _ = fs::remove_file(self.dir.join(format!("{name}.tmp")));
    }

    /// Deletes a state file, its temporary file, and quarantined copies.
    pub(crate) fn remove_all(&self, kind: FileKind) {
        self.remove_current(kind);
        let prefix = format!("{}.corrupt-", kind.file_name());
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.filter_map(Result::ok) {
                if entry.file_name().to_string_lossy().starts_with(&prefix) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Payload {
        values: Vec<String>,
    }

    fn payload(values: &[&str]) -> Payload {
        Payload {
            values: values.iter().map(|v| (*v).to_owned()).collect(),
        }
    }

    fn store(dir: &Path) -> FileStore {
        FileStore::new(dir.to_owned(), &[7u8; 32])
    }

    fn expect_loaded(loaded: Loaded<Payload>) -> Payload {
        match loaded {
            Loaded::Value(value) => value,
            other => panic!("expected a loaded value, got {other:?}"),
        }
    }

    #[test]
    fn round_trips_and_is_not_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let files = store(dir.path());
        assert!(matches!(
            files.load::<Payload>(FileKind::Drafts),
            Loaded::Missing
        ));
        files
            .save(FileKind::Drafts, &payload(&["secret draft"]))
            .unwrap();
        let raw = fs::read(files.path(FileKind::Drafts)).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("secret draft"));
        assert_eq!(
            expect_loaded(files.load(FileKind::Drafts)),
            payload(&["secret draft"])
        );
    }

    #[test]
    fn a_crash_during_a_write_leaves_the_previous_version() {
        for crash_after in [WriteStep::TempWritten, WriteStep::TempSynced] {
            let dir = tempfile::tempdir().unwrap();
            let files = store(dir.path());
            files.save(FileKind::Journal, &payload(&["old"])).unwrap();

            let result = files.save_with_hook(FileKind::Journal, &payload(&["new"]), &mut |step| {
                if step == crash_after {
                    Err(io::Error::other("simulated crash"))
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err());
            assert_eq!(
                expect_loaded(files.load(FileKind::Journal)),
                payload(&["old"]),
                "{crash_after:?}"
            );
            // The leftover temporary file does not disturb the next write.
            files.save(FileKind::Journal, &payload(&["newer"])).unwrap();
            assert_eq!(
                expect_loaded(files.load(FileKind::Journal)),
                payload(&["newer"])
            );
        }
    }

    #[test]
    fn a_crash_after_the_rename_shows_the_new_version() {
        let dir = tempfile::tempdir().unwrap();
        let files = store(dir.path());
        files.save(FileKind::Journal, &payload(&["old"])).unwrap();
        let _ = files.save_with_hook(FileKind::Journal, &payload(&["new"]), &mut |step| {
            if step == WriteStep::Renamed {
                Err(io::Error::other("crash before the directory sync"))
            } else {
                Ok(())
            }
        });
        assert_eq!(
            expect_loaded(files.load(FileKind::Journal)),
            payload(&["new"])
        );
    }

    #[test]
    fn truncated_files_are_set_aside_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let files = store(dir.path());
        files.save(FileKind::Journal, &payload(&["entry"])).unwrap();
        let path = files.path(FileKind::Journal);
        let bytes = fs::read(&path).unwrap();
        fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();

        let Loaded::Corrupt(message) = files.load::<Payload>(FileKind::Journal) else {
            panic!("expected corruption to be reported");
        };
        assert!(message.contains("set aside"), "{message}");
        assert!(
            !path.exists(),
            "the damaged file is moved, not left in place"
        );
        let aside: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .collect();
        assert_eq!(aside.len(), 1);
        assert!(matches!(
            files.load::<Payload>(FileKind::Journal),
            Loaded::Missing
        ));
    }

    #[test]
    fn tampering_and_the_wrong_key_fail_authentication() {
        let dir = tempfile::tempdir().unwrap();
        let files = store(dir.path());
        files.save(FileKind::Drafts, &payload(&["x"])).unwrap();
        let path = files.path(FileKind::Drafts);
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            files.load::<Payload>(FileKind::Drafts),
            Loaded::Corrupt(_)
        ));

        files.save(FileKind::Drafts, &payload(&["y"])).unwrap();
        let other_key = FileStore::new(dir.path().to_owned(), &[9u8; 32]);
        assert!(matches!(
            other_key.load::<Payload>(FileKind::Drafts),
            Loaded::Corrupt(_)
        ));
    }

    #[test]
    fn a_file_cannot_be_loaded_as_another_kind() {
        let dir = tempfile::tempdir().unwrap();
        let files = store(dir.path());
        files.save(FileKind::Drafts, &payload(&["x"])).unwrap();
        fs::copy(files.path(FileKind::Drafts), files.path(FileKind::Journal)).unwrap();
        assert!(matches!(
            files.load::<Payload>(FileKind::Journal),
            Loaded::Corrupt(_)
        ));
    }

    #[test]
    fn newer_formats_are_refused_without_being_modified() {
        let dir = tempfile::tempdir().unwrap();
        let files = store(dir.path());
        files.save(FileKind::Journal, &payload(&["x"])).unwrap();
        let path = files.path(FileKind::Journal);
        let mut bytes = fs::read(&path).unwrap();
        bytes[4..6].copy_from_slice(&(FORMAT + 1).to_le_bytes());
        fs::write(&path, &bytes).unwrap();

        assert!(
            matches!(files.load::<Payload>(FileKind::Journal), Loaded::NewerFormat(f) if f == FORMAT + 1)
        );
        assert_eq!(fs::read(&path).unwrap(), bytes, "the file is untouched");
    }

    #[test]
    fn oversized_files_are_rejected_on_load_and_save() {
        let dir = tempfile::tempdir().unwrap();
        let files = store(dir.path());
        files.save(FileKind::Cache, &payload(&["small"])).unwrap();

        let huge = payload(&[&"x".repeat(FileKind::Cache.max_bytes() as usize)]);
        assert!(matches!(
            files.save(FileKind::Cache, &huge),
            Err(FileError::TooLarge { .. })
        ));
        assert_eq!(
            expect_loaded(files.load(FileKind::Cache)),
            payload(&["small"]),
            "unchanged"
        );

        fs::write(
            files.path(FileKind::Cache),
            vec![0u8; FileKind::Cache.max_bytes() as usize + 1],
        )
        .unwrap();
        assert!(matches!(
            files.load::<Payload>(FileKind::Cache),
            Loaded::Corrupt(_)
        ));
    }

    #[test]
    fn remove_all_deletes_the_file_its_temp_and_quarantined_copies() {
        let dir = tempfile::tempdir().unwrap();
        let files = store(dir.path());
        files.save(FileKind::Journal, &payload(&["x"])).unwrap();
        fs::write(dir.path().join("journal.enc.tmp"), b"partial").unwrap();
        fs::write(dir.path().join("journal.enc.corrupt-1"), b"old").unwrap();
        files.remove_all(FileKind::Journal);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    #[cfg(unix)]
    fn state_files_are_private_to_the_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let files = store(dir.path());
        files.save(FileKind::Drafts, &payload(&["x"])).unwrap();
        let mode = fs::metadata(files.path(FileKind::Drafts))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "group and others have no access");
    }
}
