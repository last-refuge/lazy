//! Where the bytes of attachments live (llm-docs/DECISIONS.md, D17).
//!
//! [`AttachmentStore`] is the whole contract: stage an upload from a stream while hashing it,
//! commit a staged file under its name, read an object back as a stream, delete, and list (for the
//! sweep). Postgres keeps the metadata and who may read each file; this keeps only bytes, so
//! another backend (an object store) can replace [`FileStore`] without changing the schema or the
//! wire.
//!
//! An upload takes two steps so that a crash can never leave a partial file under a real name and
//! two uploads of one id can never mix their bytes: [`AttachmentStore::stage`] streams the body
//! into a private staging file (hashing it, enforcing the limit, flushing it to disk), and
//! [`AttachmentStore::commit`] atomically moves it to its place once the caller has decided it is
//! wanted.

use std::collections::HashMap;
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;
use std::time::SystemTime;

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWriteExt};
use uuid::Uuid;

/// How many leading bytes of an upload are kept for recognising its type.
pub const PREFIX_BYTES: usize = 512;

/// Which kind of file. Attachments and custom emoji are named by ids that clients choose, so each
/// kind has a namespace of its own: an emoji whose id equals an attachment's is a different object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    /// An attachment's bytes.
    Original,
    /// An attachment's thumbnail.
    Thumbnail,
    /// A custom emoji's PNG.
    Emoji,
}

/// The name of one stored object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectKey {
    pub id: Uuid,
    pub kind: ObjectKind,
}

impl ObjectKey {
    pub fn original(id: Uuid) -> Self {
        Self {
            id,
            kind: ObjectKind::Original,
        }
    }

    pub fn thumbnail(id: Uuid) -> Self {
        Self {
            id,
            kind: ObjectKind::Thumbnail,
        }
    }

    pub fn emoji(id: Uuid) -> Self {
        Self {
            id,
            kind: ObjectKind::Emoji,
        }
    }
}

/// Why staging an upload failed.
#[derive(Debug)]
pub enum StoreError {
    /// The body is longer than the limit.
    TooLarge,
    /// The body ended early, or the disk failed.
    Io(io::Error),
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// An upload that has been received in full and is waiting to be committed or discarded.
#[derive(Debug, Clone)]
pub struct Staged {
    /// Names the staged file to the store.
    pub token: String,
    pub size: u64,
    pub sha256: [u8; 32],
    /// The first bytes, up to [`PREFIX_BYTES`].
    pub prefix: Vec<u8>,
}

/// Something that can be read on a blocking thread, for decoding an image.
pub trait BlockingSource: Read + Seek + Send {}

impl<T: Read + Seek + Send> BlockingSource for T {}

/// An object opened for reading.
pub struct StoredObject {
    pub size: u64,
    pub reader: Pin<Box<dyn AsyncRead + Send>>,
}

/// A file the store holds, for the sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEntry {
    pub what: EntryKind,
    pub modified: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    /// A committed object.
    Object(ObjectKey),
    /// A staged upload that was never committed or discarded.
    Staged(String),
}

pub trait AttachmentStore: Send + Sync + 'static {
    /// Streams `body` into a private staging file, hashing it and keeping its first bytes, and
    /// flushes it to disk. Fails with [`StoreError::TooLarge`] as soon as more than `limit` bytes
    /// have arrived. Nothing is left behind on failure. `id` only names the staging file.
    fn stage<'a>(
        &'a self,
        id: Uuid,
        body: BoxStream<'a, io::Result<Bytes>>,
        limit: u64,
    ) -> BoxFuture<'a, Result<Staged, StoreError>>;

    /// A blocking reader of a staged file, for decoding an image without loading it whole.
    fn staged_source(&self, token: &str) -> io::Result<Box<dyn BlockingSource>>;

    /// Moves a staged file to `key`, atomically, and makes the move durable. Replaces what was
    /// there (the same bytes: ids are written once).
    fn commit<'a>(&'a self, token: &'a str, key: ObjectKey) -> BoxFuture<'a, io::Result<()>>;

    /// Drops a staged file. Never fails: what cannot be removed now is left to the sweep.
    fn discard<'a>(&'a self, token: &'a str) -> BoxFuture<'a, ()>;

    /// Stores a small object (a thumbnail) in one step, atomically and durably.
    fn put_small<'a>(&'a self, key: ObjectKey, bytes: Vec<u8>) -> BoxFuture<'a, io::Result<()>>;

    /// Opens an object for streaming, or `None` if there is none.
    fn open<'a>(&'a self, key: ObjectKey) -> BoxFuture<'a, io::Result<Option<StoredObject>>>;

    /// Removes an object; whether there was one.
    fn delete<'a>(&'a self, key: ObjectKey) -> BoxFuture<'a, io::Result<bool>>;

    /// Everything stored, committed or staged.
    fn list<'a>(&'a self) -> BoxFuture<'a, io::Result<Vec<StoredEntry>>>;
}

// --- Filesystem --------------------------------------------------------------------------------

/// Files under a directory: `{dir}/{first two hex digits of the id}/{id}` (and `{id}.thumb` for a
/// thumbnail, `{id}.emoji` for a custom emoji), with uploads staged in `{dir}/tmp/{id}.{nonce}.part`.
pub struct FileStore {
    root: PathBuf,
}

impl FileStore {
    /// Opens (creating it if needed) the directory.
    pub fn open_dir(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join("tmp"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_of(&self, key: ObjectKey) -> PathBuf {
        let name = key.id.simple().to_string();
        let file = match key.kind {
            ObjectKind::Original => name.clone(),
            ObjectKind::Thumbnail => format!("{name}.thumb"),
            ObjectKind::Emoji => format!("{name}.emoji"),
        };
        self.root.join(&name[..2]).join(file)
    }

    fn staged_path(&self, token: &str) -> PathBuf {
        self.root.join("tmp").join(format!("{token}.part"))
    }
}

/// Flushes a directory's entries to disk, so that a rename in it survives a crash.
async fn sync_dir(dir: &Path) -> io::Result<()> {
    tokio::fs::File::open(dir).await?.sync_all().await
}

fn staging_nonce() -> io::Result<String> {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(random.iter().map(|b| format!("{b:02x}")).collect())
}

impl AttachmentStore for FileStore {
    fn stage<'a>(
        &'a self,
        id: Uuid,
        mut body: BoxStream<'a, io::Result<Bytes>>,
        limit: u64,
    ) -> BoxFuture<'a, Result<Staged, StoreError>> {
        Box::pin(async move {
            let token = format!("{}.{}", id.simple(), staging_nonce()?);
            let path = self.staged_path(&token);
            let result = async {
                let mut file = tokio::fs::File::create(&path).await?;
                let mut hasher = Sha256::new();
                let mut size = 0u64;
                let mut prefix = Vec::with_capacity(PREFIX_BYTES);
                while let Some(chunk) = body.next().await {
                    let chunk = chunk?;
                    size += chunk.len() as u64;
                    if size > limit {
                        return Err(StoreError::TooLarge);
                    }
                    hasher.update(&chunk);
                    if prefix.len() < PREFIX_BYTES {
                        let take = (PREFIX_BYTES - prefix.len()).min(chunk.len());
                        prefix.extend_from_slice(&chunk[..take]);
                    }
                    file.write_all(&chunk).await?;
                }
                file.flush().await?;
                file.sync_all().await?;
                Ok(Staged {
                    token: token.clone(),
                    size,
                    sha256: hasher.finalize().into(),
                    prefix,
                })
            }
            .await;
            if result.is_err() {
                let _ = tokio::fs::remove_file(&path).await;
            }
            result
        })
    }

    fn staged_source(&self, token: &str) -> io::Result<Box<dyn BlockingSource>> {
        Ok(Box::new(std::fs::File::open(self.staged_path(token))?))
    }

    fn commit<'a>(&'a self, token: &'a str, key: ObjectKey) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let target = self.path_of(key);
            let dir = target
                .parent()
                .ok_or_else(|| io::Error::other("object path has no directory"))?;
            tokio::fs::create_dir_all(dir).await?;
            tokio::fs::rename(self.staged_path(token), &target).await?;
            sync_dir(dir).await
        })
    }

    fn discard<'a>(&'a self, token: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let _ = tokio::fs::remove_file(self.staged_path(token)).await;
        })
    }

    fn put_small<'a>(&'a self, key: ObjectKey, bytes: Vec<u8>) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let token = format!("{}.{}.small", key.id.simple(), staging_nonce()?);
            let staged = self.root.join("tmp").join(format!("{token}.part"));
            let written = async {
                let mut file = tokio::fs::File::create(&staged).await?;
                file.write_all(&bytes).await?;
                file.flush().await?;
                file.sync_all().await
            }
            .await;
            if let Err(error) = written {
                let _ = tokio::fs::remove_file(&staged).await;
                return Err(error);
            }
            let target = self.path_of(key);
            let dir = target
                .parent()
                .ok_or_else(|| io::Error::other("object path has no directory"))?;
            tokio::fs::create_dir_all(dir).await?;
            tokio::fs::rename(&staged, &target).await?;
            sync_dir(dir).await
        })
    }

    fn open<'a>(&'a self, key: ObjectKey) -> BoxFuture<'a, io::Result<Option<StoredObject>>> {
        Box::pin(async move {
            match tokio::fs::File::open(self.path_of(key)).await {
                Ok(file) => {
                    let size = file.metadata().await?.len();
                    Ok(Some(StoredObject {
                        size,
                        reader: Box::pin(file),
                    }))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            }
        })
    }

    fn delete<'a>(&'a self, key: ObjectKey) -> BoxFuture<'a, io::Result<bool>> {
        Box::pin(async move {
            match tokio::fs::remove_file(self.path_of(key)).await {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error),
            }
        })
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, io::Result<Vec<StoredEntry>>> {
        let root = self.root.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || list_directory(&root))
                .await
                .map_err(|e| io::Error::other(e.to_string()))?
        })
    }
}

/// Walks the store's directory. Names that are not ours are ignored, never removed.
fn list_directory(root: &Path) -> io::Result<Vec<StoredEntry>> {
    let mut entries = Vec::new();
    for shard in std::fs::read_dir(root)? {
        let shard = shard?;
        if !shard.file_type()?.is_dir() {
            continue;
        }
        let shard_name = shard.file_name().to_string_lossy().into_owned();
        for file in std::fs::read_dir(shard.path())? {
            let file = file?;
            let name = file.file_name().to_string_lossy().into_owned();
            let modified = file.metadata()?.modified()?;
            let what = if shard_name == "tmp" {
                match name.strip_suffix(".part") {
                    Some(token) => EntryKind::Staged(token.to_owned()),
                    None => continue,
                }
            } else {
                let (stem, kind) = if let Some(stem) = name.strip_suffix(".thumb") {
                    (stem, ObjectKind::Thumbnail)
                } else if let Some(stem) = name.strip_suffix(".emoji") {
                    (stem, ObjectKind::Emoji)
                } else {
                    (name.as_str(), ObjectKind::Original)
                };
                match Uuid::try_parse(stem) {
                    Ok(id) if id.simple().to_string().starts_with(&shard_name) => {
                        EntryKind::Object(ObjectKey { id, kind })
                    }
                    _ => continue,
                }
            };
            entries.push(StoredEntry { what, modified });
        }
    }
    Ok(entries)
}

// --- Memory ------------------------------------------------------------------------------------

/// Objects held in memory, for tests that do not look at the disk. Staged uploads are collected
/// whole, which the file store never does.
#[derive(Default)]
pub struct MemoryStore {
    state: Mutex<MemoryState>,
}

#[derive(Default)]
struct MemoryState {
    objects: HashMap<ObjectKey, (Vec<u8>, SystemTime)>,
    staged: HashMap<String, (Vec<u8>, SystemTime)>,
    next: u64,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl AttachmentStore for MemoryStore {
    fn stage<'a>(
        &'a self,
        id: Uuid,
        mut body: BoxStream<'a, io::Result<Bytes>>,
        limit: u64,
    ) -> BoxFuture<'a, Result<Staged, StoreError>> {
        Box::pin(async move {
            let mut bytes = Vec::new();
            while let Some(chunk) = body.next().await {
                let chunk = chunk?;
                if (bytes.len() + chunk.len()) as u64 > limit {
                    return Err(StoreError::TooLarge);
                }
                bytes.extend_from_slice(&chunk);
            }
            let mut state = self.lock();
            state.next += 1;
            let token = format!("{}.{}", id.simple(), state.next);
            let staged = Staged {
                token: token.clone(),
                size: bytes.len() as u64,
                sha256: Sha256::digest(&bytes).into(),
                prefix: bytes[..bytes.len().min(PREFIX_BYTES)].to_vec(),
            };
            state.staged.insert(token, (bytes, SystemTime::now()));
            Ok(staged)
        })
    }

    fn staged_source(&self, token: &str) -> io::Result<Box<dyn BlockingSource>> {
        let state = self.lock();
        let (bytes, _) = state
            .staged
            .get(token)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        Ok(Box::new(io::Cursor::new(bytes.clone())))
    }

    fn commit<'a>(&'a self, token: &'a str, key: ObjectKey) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let mut state = self.lock();
            let (bytes, _) = state
                .staged
                .remove(token)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            state.objects.insert(key, (bytes, SystemTime::now()));
            Ok(())
        })
    }

    fn discard<'a>(&'a self, token: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.lock().staged.remove(token);
        })
    }

    fn put_small<'a>(&'a self, key: ObjectKey, bytes: Vec<u8>) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.lock().objects.insert(key, (bytes, SystemTime::now()));
            Ok(())
        })
    }

    fn open<'a>(&'a self, key: ObjectKey) -> BoxFuture<'a, io::Result<Option<StoredObject>>> {
        Box::pin(async move {
            Ok(self
                .lock()
                .objects
                .get(&key)
                .map(|(bytes, _)| StoredObject {
                    size: bytes.len() as u64,
                    reader: Box::pin(io::Cursor::new(bytes.clone())),
                }))
        })
    }

    fn delete<'a>(&'a self, key: ObjectKey) -> BoxFuture<'a, io::Result<bool>> {
        Box::pin(async move { Ok(self.lock().objects.remove(&key).is_some()) })
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, io::Result<Vec<StoredEntry>>> {
        Box::pin(async move {
            let state = self.lock();
            let mut entries: Vec<StoredEntry> = state
                .objects
                .iter()
                .map(|(key, (_, modified))| StoredEntry {
                    what: EntryKind::Object(*key),
                    modified: *modified,
                })
                .collect();
            entries.extend(
                state
                    .staged
                    .iter()
                    .map(|(token, (_, modified))| StoredEntry {
                        what: EntryKind::Staged(token.clone()),
                        modified: *modified,
                    }),
            );
            Ok(entries)
        })
    }
}

#[cfg(test)]
mod tests {
    use futures_util::stream;
    use tokio::io::AsyncReadExt;

    use super::*;

    fn body(chunks: &[&[u8]]) -> BoxStream<'static, io::Result<Bytes>> {
        let chunks: Vec<io::Result<Bytes>> = chunks
            .iter()
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        Box::pin(stream::iter(chunks))
    }

    async fn read_all(store: &dyn AttachmentStore, key: ObjectKey) -> Option<Vec<u8>> {
        let mut object = store.open(key).await.unwrap()?;
        let mut bytes = Vec::new();
        object.reader.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes.len() as u64, object.size);
        Some(bytes)
    }

    /// The same behavior from both implementations.
    async fn behaves(store: &dyn AttachmentStore) {
        let id = Uuid::now_v7();
        let key = ObjectKey::original(id);
        // Stage, hash, commit, read back.
        let staged = store
            .stage(id, body(&[b"hello ", b"world"]), 100)
            .await
            .unwrap();
        assert_eq!(staged.size, 11);
        assert_eq!(staged.sha256[..], Sha256::digest(b"hello world")[..]);
        assert_eq!(staged.prefix, b"hello world");
        assert!(
            read_all(store, key).await.is_none(),
            "not there until committed"
        );
        let mut source = store.staged_source(&staged.token).unwrap();
        let mut seen = String::new();
        source.read_to_string(&mut seen).unwrap();
        assert_eq!(seen, "hello world");
        store.commit(&staged.token, key).await.unwrap();
        assert_eq!(read_all(store, key).await.unwrap(), b"hello world");
        // The prefix is the first bytes only.
        let long = vec![7u8; PREFIX_BYTES * 3];
        let staged = store.stage(id, body(&[&long]), 10_000).await.unwrap();
        assert_eq!(staged.prefix.len(), PREFIX_BYTES);
        store.discard(&staged.token).await;
        // The limit is enforced while streaming, and nothing is left staged.
        let before = store.list().await.unwrap().len();
        let refused = store.stage(id, body(&[b"12345", b"67890", b"x"]), 10).await;
        assert!(matches!(refused, Err(StoreError::TooLarge)));
        let exact = store
            .stage(id, body(&[b"12345", b"67890"]), 10)
            .await
            .unwrap();
        assert_eq!(exact.size, 10);
        store.discard(&exact.token).await;
        // A body that fails midway leaves nothing.
        let broken: BoxStream<'static, io::Result<Bytes>> = Box::pin(stream::iter(vec![
            Ok(Bytes::from_static(b"abc")),
            Err(io::Error::from(io::ErrorKind::ConnectionReset)),
        ]));
        assert!(matches!(
            store.stage(id, broken, 10).await,
            Err(StoreError::Io(_))
        ));
        assert_eq!(store.list().await.unwrap().len(), before);
        // Small objects, listing, deleting.
        let thumb = ObjectKey::thumbnail(id);
        store.put_small(thumb, b"tiny".to_vec()).await.unwrap();
        assert_eq!(read_all(store, thumb).await.unwrap(), b"tiny");
        let listed: Vec<EntryKind> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.what)
            .collect();
        assert!(
            listed.contains(&EntryKind::Object(key)) && listed.contains(&EntryKind::Object(thumb))
        );
        assert!(store.delete(key).await.unwrap());
        assert!(!store.delete(key).await.unwrap());
        assert!(read_all(store, key).await.is_none());
        assert!(store.delete(thumb).await.unwrap());
        // An emoji with the same id as a file is another object: neither reads, replaces, lists as,
        // or deletes the other.
        store.put_small(key, b"file".to_vec()).await.unwrap();
        let emoji = ObjectKey::emoji(id);
        assert!(read_all(store, emoji).await.is_none());
        store.put_small(emoji, b"emoji".to_vec()).await.unwrap();
        assert_eq!(read_all(store, key).await.unwrap(), b"file");
        assert_eq!(read_all(store, emoji).await.unwrap(), b"emoji");
        let listed: Vec<EntryKind> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.what)
            .collect();
        assert!(
            listed.contains(&EntryKind::Object(key)) && listed.contains(&EntryKind::Object(emoji))
        );
        assert!(store.delete(emoji).await.unwrap());
        assert_eq!(read_all(store, key).await.unwrap(), b"file");
        assert!(store.delete(key).await.unwrap());
    }

    #[tokio::test]
    async fn the_memory_store_behaves() {
        behaves(&MemoryStore::new()).await;
    }

    #[tokio::test]
    async fn the_file_store_behaves_and_lays_files_out_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::open_dir(dir.path()).unwrap();
        behaves(&store).await;
        let id = Uuid::now_v7();
        let staged = store.stage(id, body(&[b"x"]), 10).await.unwrap();
        // Staged under tmp, with the id in the name.
        let staged_path = dir
            .path()
            .join("tmp")
            .join(format!("{}.part", staged.token));
        assert!(staged_path.exists() && staged.token.starts_with(&id.simple().to_string()));
        store
            .commit(&staged.token, ObjectKey::original(id))
            .await
            .unwrap();
        assert!(!staged_path.exists());
        let simple = id.simple().to_string();
        assert!(dir.path().join(&simple[..2]).join(&simple).is_file());
        // Foreign files in the directory are not ours to list.
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        std::fs::create_dir(dir.path().join("zz")).unwrap();
        std::fs::write(dir.path().join("zz").join("stray"), "x").unwrap();
        let listed = store.list().await.unwrap();
        assert!(
            listed
                .iter()
                .all(|entry| matches!(entry.what, EntryKind::Object(key) if key.id == id))
        );
    }
}
