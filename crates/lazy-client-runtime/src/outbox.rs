//! The outbox: encrypted copies of files waiting to be uploaded (slice 3A, D17).
//!
//! A file the person attaches is copied here before anything else happens, so the upload does not
//! depend on the original (which may be edited, moved, or deleted) and survives a restart. A copy
//! is named by its attachment id, written under a temporary name, flushed, and renamed into place,
//! so a crash leaves a whole copy or none. Copies are deleted when their message is sent or
//! discarded, at sign-out, and at start for any copy no journal entry names.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lazy_client_core::{FileSource, StagedFile, TransferError};
use lazy_domain::AttachmentId;
use lazy_domain::attachment::MAX_ATTACHMENT_BYTES;

use crate::blob::{BlobKey, BlobKind, BlobReader, BlobWriter, CHUNK, open_file};

const EXTENSION: &str = "bin";
const PART_EXTENSION: &str = "part";

pub(crate) struct Outbox {
    dir: PathBuf,
    key: Arc<BlobKey>,
}

/// What the system said, without the "(os error N)" tail.
pub(crate) fn describe_io(error: &io::Error) -> String {
    let text = error.to_string();
    match text.rfind(" (os error ") {
        Some(at) => text[..at].to_owned(),
        None => text,
    }
}

fn unreadable(error: io::Error) -> TransferError {
    TransferError::Unreadable(describe_io(&error))
}

fn disk(error: io::Error) -> TransferError {
    TransferError::Disk(describe_io(&error))
}

/// Creates a private directory (and its parents).
pub(crate) fn private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Opens a new private file for writing.
pub(crate) fn create_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

#[cfg(unix)]
pub(crate) fn sync_dir(dir: &Path) {
    if let Ok(dir) = File::open(dir) {
        let _ = dir.sync_all();
    }
}

#[cfg(not(unix))]
pub(crate) fn sync_dir(_dir: &Path) {}

impl Outbox {
    pub(crate) fn new(profile_dir: &Path, key: Arc<BlobKey>) -> Self {
        Self {
            dir: profile_dir.join("outbox"),
            key,
        }
    }

    fn path(&self, id: AttachmentId) -> PathBuf {
        self.dir.join(format!("{id}.{EXTENSION}"))
    }

    fn part_path(&self, id: AttachmentId) -> PathBuf {
        self.dir.join(format!("{id}.{PART_EXTENSION}"))
    }

    /// Copies a file (or pasted bytes) into the outbox. Checks the size against the limit first
    /// (from the file's metadata, so a huge file is refused before a byte is copied) and again
    /// while copying, because a file can grow. Stops early, leaving nothing behind, when `cancel`
    /// is set.
    pub(crate) fn stage(
        &self,
        id: AttachmentId,
        source: &FileSource,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<StagedFile, TransferError> {
        let part = self.part_path(id);
        let result = self.copy_in(&part, source, cancelled);
        match result {
            Ok(size) => {
                if cancelled() {
                    let _ = fs::remove_file(&part);
                    return Err(TransferError::Cancelled);
                }
                if let Err(error) = fs::rename(&part, self.path(id)) {
                    let _ = fs::remove_file(&part);
                    return Err(disk(error));
                }
                sync_dir(&self.dir);
                Ok(StagedFile { size })
            }
            Err(error) => {
                let _ = fs::remove_file(&part);
                Err(error)
            }
        }
    }

    fn copy_in(
        &self,
        part: &Path,
        source: &FileSource,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<u64, TransferError> {
        // Look at the original first: a file that is too large or not a file costs nothing.
        let mut input = match source {
            FileSource::Path(path) => {
                let metadata = fs::metadata(path).map_err(unreadable)?;
                if !metadata.is_file() {
                    return Err(TransferError::Unreadable("it is not a file".to_owned()));
                }
                if metadata.len() > MAX_ATTACHMENT_BYTES {
                    return Err(TransferError::TooLarge {
                        size: metadata.len(),
                    });
                }
                Some(File::open(path).map_err(unreadable)?)
            }
            FileSource::Bytes(bytes) => {
                if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
                    return Err(TransferError::TooLarge {
                        size: bytes.len() as u64,
                    });
                }
                None
            }
        };
        private_dir(&self.dir).map_err(disk)?;
        let file = create_private(part).map_err(disk)?;
        let mut writer = BlobWriter::new(BufWriter::new(file), self.key.clone(), BlobKind::Outbox)
            .map_err(disk)?;
        let mut total = 0u64;
        match (&mut input, source) {
            (Some(input), _) => {
                let mut buffer = vec![0u8; CHUNK];
                loop {
                    if cancelled() {
                        return Err(TransferError::Cancelled);
                    }
                    let read = input.read(&mut buffer).map_err(unreadable)?;
                    if read == 0 {
                        break;
                    }
                    total += read as u64;
                    if total > MAX_ATTACHMENT_BYTES {
                        return Err(TransferError::TooLarge { size: total });
                    }
                    writer.push(&buffer[..read]).map_err(disk)?;
                }
            }
            (None, FileSource::Bytes(bytes)) => {
                for piece in bytes.as_slice().chunks(CHUNK) {
                    if cancelled() {
                        return Err(TransferError::Cancelled);
                    }
                    writer.push(piece).map_err(disk)?;
                }
                total = bytes.len() as u64;
            }
            (None, FileSource::Path(_)) => {}
        }
        let out = writer.finish().map_err(disk)?;
        let file = out.into_inner().map_err(|e| disk(e.into_error()))?;
        file.sync_all().map_err(disk)?;
        Ok(total)
    }

    /// Opens a copy for reading, with its size.
    pub(crate) fn open(
        &self,
        id: AttachmentId,
    ) -> Result<(BlobReader<io::BufReader<File>>, u64), TransferError> {
        open_file(&self.path(id), self.key.clone(), BlobKind::Outbox).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                TransferError::Missing
            } else {
                unreadable(error)
            }
        })
    }

    /// Deletes a copy (and a half-made one).
    pub(crate) fn remove(&self, id: AttachmentId) {
        let _ = fs::remove_file(self.path(id));
        let _ = fs::remove_file(self.part_path(id));
    }

    /// Deletes every copy that is not in `keep`, and every half-made one.
    pub(crate) fn sweep(&self, keep: &HashSet<AttachmentId>) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let kept = path.extension().is_some_and(|ext| ext == EXTENSION)
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|stem| stem.parse::<AttachmentId>().ok())
                    .is_some_and(|id| keep.contains(&id));
            if !kept {
                let _ = fs::remove_file(path);
            }
        }
    }

    /// Deletes everything (sign-out).
    pub(crate) fn clear(&self) {
        let _ = fs::remove_dir_all(&self.dir);
    }

    /// The ids of the copies on disk, for tests.
    #[cfg(test)]
    pub(crate) fn held(&self) -> Vec<AttachmentId> {
        let mut ids: Vec<AttachmentId> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == EXTENSION))
            .filter_map(|e| e.path().file_stem()?.to_str()?.parse().ok())
            .collect();
        ids.sort();
        ids
    }
}

#[cfg(test)]
mod tests {
    use lazy_client_core::FileBytes;

    use super::*;

    fn outbox(dir: &Path) -> Outbox {
        Outbox::new(dir, Arc::new(BlobKey::new(&[5; 32])))
    }

    fn read_back(outbox: &Outbox, id: AttachmentId) -> Vec<u8> {
        let (mut reader, size) = outbox.open(id).unwrap();
        let mut out = Vec::new();
        while let Some(chunk) = reader.next_chunk().unwrap() {
            out.extend_from_slice(&chunk);
        }
        assert_eq!(out.len() as u64, size);
        out
    }

    #[test]
    fn a_file_is_copied_encrypted_and_read_back_whole() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("photo.png");
        let data: Vec<u8> = (0..200_000).map(|n| (n % 253) as u8).collect();
        fs::write(&source, &data).unwrap();
        let outbox = outbox(dir.path());
        let id = AttachmentId::generate();
        let staged = outbox
            .stage(id, &FileSource::Path(source.clone()), &|| false)
            .unwrap();
        assert_eq!(staged.size, 200_000);
        assert_eq!(read_back(&outbox, id), data);
        // The copy on disk is not the file.
        let on_disk = fs::read(outbox.path(id)).unwrap();
        assert!(!on_disk.windows(64).any(|w| w == &data[1000..1064]));
        // And the original can go.
        fs::remove_file(source).unwrap();
        assert_eq!(read_back(&outbox, id), data);
    }

    #[test]
    fn pasted_bytes_are_copied_too() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = outbox(dir.path());
        let id = AttachmentId::generate();
        let staged = outbox
            .stage(
                id,
                &FileSource::Bytes(FileBytes::new(vec![9; 1000])),
                &|| false,
            )
            .unwrap();
        assert_eq!(staged.size, 1000);
        assert_eq!(read_back(&outbox, id), vec![9; 1000]);
    }

    #[test]
    fn an_empty_file_is_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("empty.txt");
        fs::write(&source, b"").unwrap();
        let outbox = outbox(dir.path());
        let id = AttachmentId::generate();
        assert_eq!(
            outbox
                .stage(id, &FileSource::Path(source), &|| false)
                .unwrap()
                .size,
            0
        );
        assert!(read_back(&outbox, id).is_empty());
    }

    #[test]
    fn a_missing_file_a_directory_and_a_huge_file_are_refused_before_any_copy() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = outbox(dir.path());
        let id = AttachmentId::generate();
        let cancel = || false;
        let missing = outbox.stage(id, &FileSource::Path(dir.path().join("nope")), &cancel);
        assert!(
            matches!(missing, Err(TransferError::Unreadable(_))),
            "{missing:?}"
        );
        let directory = outbox.stage(id, &FileSource::Path(dir.path().to_owned()), &cancel);
        assert!(matches!(directory, Err(TransferError::Unreadable(_))));
        // A sparse file just over the limit: its metadata says it is too large.
        let big = dir.path().join("big.bin");
        let file = File::create(&big).unwrap();
        file.set_len(MAX_ATTACHMENT_BYTES + 1).unwrap();
        let huge = outbox.stage(id, &FileSource::Path(big), &cancel);
        assert_eq!(
            huge,
            Err(TransferError::TooLarge {
                size: MAX_ATTACHMENT_BYTES + 1
            })
        );
        assert!(outbox.held().is_empty());
        assert!(!outbox.part_path(id).exists());
    }

    #[test]
    fn a_cancelled_copy_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("a.bin");
        fs::write(&source, vec![1u8; 300_000]).unwrap();
        let outbox = outbox(dir.path());
        let id = AttachmentId::generate();
        let cancel = || true;
        assert_eq!(
            outbox.stage(id, &FileSource::Path(source), &cancel),
            Err(TransferError::Cancelled)
        );
        assert!(outbox.held().is_empty());
        assert!(!outbox.part_path(id).exists());
    }

    #[test]
    fn a_copy_that_is_not_there_is_missing_and_a_damaged_one_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = outbox(dir.path());
        let id = AttachmentId::generate();
        assert!(matches!(outbox.open(id), Err(TransferError::Missing)));
        outbox
            .stage(
                id,
                &FileSource::Bytes(FileBytes::new(vec![3; 100])),
                &|| false,
            )
            .unwrap();
        let path = outbox.path(id);
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&path, bytes).unwrap();
        let (mut reader, _) = outbox.open(id).unwrap();
        assert!(reader.next_chunk().is_err());
        fs::write(&path, b"short").unwrap();
        assert!(matches!(outbox.open(id), Err(TransferError::Unreadable(_))));
    }

    #[test]
    fn sweeping_keeps_only_the_named_copies_and_clearing_removes_all() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = outbox(dir.path());
        let cancel = || false;
        let ids: Vec<AttachmentId> = (0..3).map(|_| AttachmentId::generate()).collect();
        for id in &ids {
            outbox
                .stage(
                    *id,
                    &FileSource::Bytes(FileBytes::new(vec![1; 10])),
                    &cancel,
                )
                .unwrap();
        }
        fs::write(outbox.part_path(AttachmentId::generate()), b"half").unwrap();
        fs::write(outbox.dir.join("stray.txt"), b"x").unwrap();
        outbox.sweep(&HashSet::from([ids[1]]));
        assert_eq!(outbox.held(), vec![ids[1]]);
        assert_eq!(fs::read_dir(&outbox.dir).unwrap().count(), 1);
        outbox.remove(ids[1]);
        assert!(outbox.held().is_empty());
        outbox
            .stage(
                ids[0],
                &FileSource::Bytes(FileBytes::new(vec![1; 10])),
                &cancel,
            )
            .unwrap();
        outbox.clear();
        assert!(!outbox.dir.exists());
    }
}
