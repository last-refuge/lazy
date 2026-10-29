//! Files on messages (slice 3A, D17): what a composer holds, what a message that is still
//! uploading shows, downloads, and image previews.
//!
//! The core never reads a file. A chosen file is a [`FileSource`]: a path the runtime copies into
//! its outbox, or bytes the person pasted (a screenshot, at most [`MAX_ATTACHMENT_BYTES`]), which
//! the core passes on in an effect without looking at them. Everything else is descriptors,
//! states, and effects.

use std::path::PathBuf;
use std::sync::Arc;

use lazy_domain::attachment::MAX_ATTACHMENT_BYTES;
use lazy_domain::{AttachmentId, ConversationId, MessageId};

use crate::model::RequestError;

/// Most uploads in flight at once, across the whole app.
pub const MAX_CONCURRENT_UPLOADS: usize = 3;
/// How many times an upload whose outcome is unknown is repeated (the same id, so the server
/// treats it as the same upload) before the file is shown as failed.
pub const MAX_UPLOAD_ATTEMPTS: u32 = 3;
/// Most thumbnails the core holds ready for the screen; the oldest are dropped first and asked
/// for again when they come into view.
pub const MAX_HELD_THUMBNAILS: usize = 512;
/// Most bytes of thumbnails the core holds ready.
pub const MAX_HELD_THUMBNAIL_BYTES: usize = 32 * 1024 * 1024;
/// Most full-size images held for viewers.
pub const MAX_HELD_FULL_IMAGES: usize = 2;
/// Most custom emoji images the core holds ready for the screen, and their bytes; the oldest are
/// dropped first and asked for again when they come into view.
pub const MAX_HELD_EMOJI_IMAGES: usize = 600;
pub const MAX_HELD_EMOJI_IMAGE_BYTES: usize = 12 * 1024 * 1024;
/// Most emoji image requests waiting for a turn; the oldest are dropped first.
pub const MAX_QUEUED_EMOJI_FETCHES: usize = 128;
/// Most thumbnail and image requests in flight at once.
pub const MAX_CONCURRENT_IMAGE_FETCHES: usize = 4;
/// Most image requests waiting for a turn; the oldest are dropped first.
pub const MAX_QUEUED_IMAGE_FETCHES: usize = 64;

/// Which composer a file belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ComposerTarget {
    /// The message box of a conversation.
    Conversation(ConversationId),
    /// The reply box of the thread whose root is this message.
    Thread(MessageId),
}

/// Bytes the person pasted, shared without copying. The core never looks inside.
#[derive(Clone)]
pub struct FileBytes(Arc<[u8]>);

impl FileBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes.into())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for FileBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FileBytes({} bytes)", self.0.len())
    }
}

impl PartialEq for FileBytes {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.0 == other.0
    }
}

impl Eq for FileBytes {}

/// Where a chosen file comes from.
#[derive(Clone, PartialEq, Eq)]
pub enum FileSource {
    /// A file on disk (from the file dialog or a drop). The runtime copies it.
    Path(PathBuf),
    /// Bytes the person pasted.
    Bytes(FileBytes),
}

impl std::fmt::Debug for FileSource {
    /// A path names the person's folders and files: only that it is a path is shown.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(_) => f.write_str("Path(<path>)"),
            Self::Bytes(bytes) => write!(f, "Bytes({bytes:?})"),
        }
    }
}

/// A file the person chose to attach.
#[derive(Clone, PartialEq, Eq)]
pub struct LocalFile {
    pub source: FileSource,
    /// The name to show and send. Raw: the core makes it safe, and a name with nothing usable
    /// left becomes `file`.
    pub name: String,
}

impl std::fmt::Debug for LocalFile {
    /// The name is the person's: only its length is shown.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalFile")
            .field("source", &self.source)
            .field("name", &crate::redact::Text(&self.name))
            .finish()
    }
}

/// Where a composer file stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentState {
    /// Being copied into the outbox.
    Reading,
    /// Waiting for its turn to upload, or uploading (`sent` is 0 until bytes have gone).
    Uploading { sent: u64, total: u64 },
    /// The server holds it, waiting for the message that names it.
    Uploaded,
    /// Not uploaded. `reason` is for showing. `can_retry` says whether `Input::RetryAttachment`
    /// can try again (it cannot for a file that could not be read or was too large: remove it).
    Failed { reason: String, can_retry: bool },
}

/// One file in a composer, shown as a chip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerAttachment {
    /// The identity the upload and the message will use. Stable from the moment the file is added.
    pub id: AttachmentId,
    /// The name as it will be sent (made safe).
    pub name: String,
    /// Bytes. 0 until the runtime has read the file's size (state `Reading`); known at once for
    /// pasted bytes.
    pub size: u64,
    /// A guess from the name, for choosing an icon before the server has decided. The server's
    /// answer on the sent message is the truth.
    pub media_type_guess: String,
    pub state: AttachmentState,
    /// Where the file came from, for previewing an image chip before it is uploaded: a path to
    /// read, or the pasted bytes. `None` for files the core got back from a journal.
    pub source: Option<FileSource>,
}

/// Where one file of a message that is still being sent stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingFileState {
    /// Being copied into the outbox.
    Reading,
    /// Waiting for its turn, or uploading (`sent` is 0 until bytes have gone).
    Uploading { sent: u64, total: u64 },
    /// The server holds it.
    Uploaded,
}

/// One file of a message that is still being sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFile {
    /// The same id the sent message's attachment will have.
    pub id: AttachmentId,
    pub state: PendingFileState,
}

/// How far the files of a message that is still being sent have got, in the order they were
/// attached. Shown on the pending row ("Uploading 2 of 3 files"). Why a file did not go is the
/// row's `Delivery::Failed` reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadProgress {
    pub files: Vec<PendingFile>,
}

impl UploadProgress {
    pub fn total_files(&self) -> usize {
        self.files.len()
    }

    /// Files the server already holds.
    pub fn uploaded_files(&self) -> usize {
        self.files
            .iter()
            .filter(|file| file.state == PendingFileState::Uploaded)
            .count()
    }
}

/// The file the runtime has copied into its outbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StagedFile {
    pub size: u64,
}

/// Why a transfer or a copy did not complete, as the runtime reports it. The core words it for
/// the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferError {
    /// The server refused (`Rejected`) or its answer is unknown (`Unknown`).
    Request(RequestError),
    /// The file is larger than the limit (from its metadata, before any byte was copied).
    TooLarge { size: u64 },
    /// The file could not be read or copied; the text is the system's reason.
    Unreadable(String),
    /// The copy in the outbox is gone.
    Missing,
    /// The destination could not be written (a full disk, a missing folder); the text is the
    /// system's reason.
    Disk(String),
    /// The bytes received do not match the file's SHA-256.
    ChecksumMismatch,
    /// Stopped on request.
    Cancelled,
}

impl TransferError {
    /// A word for why, safe to log: never the system's reason, which can hold a path.
    pub fn code(&self) -> String {
        match self {
            Self::Request(error) => error.code(),
            Self::TooLarge { .. } => "too_large".to_owned(),
            Self::Unreadable(_) => "unreadable".to_owned(),
            Self::Missing => "missing".to_owned(),
            Self::Disk(_) => "disk".to_owned(),
            Self::ChecksumMismatch => "checksum_mismatch".to_owned(),
            Self::Cancelled => "cancelled".to_owned(),
        }
    }
}

/// A download's progress. Read with `ClientCore::download`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadState {
    Downloading {
        received: u64,
        total: u64,
    },
    /// Saved, and checked against the file's SHA-256, at `path`.
    Done {
        path: PathBuf,
    },
    Failed {
        reason: String,
    },
}

/// An image's bytes for the screen to decode (a thumbnail, or the file itself for a viewer).
/// Shared without copying; the core never looks inside.
#[derive(Clone)]
pub struct ImageData(Arc<[u8]>);

impl ImageData {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes.into())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for ImageData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ImageData({} bytes)", self.0.len())
    }
}

impl PartialEq for ImageData {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.0 == other.0
    }
}

impl Eq for ImageData {}

/// A thumbnail or full image. Read with `ClientCore::thumbnail` and `ClientCore::full_image`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageState {
    /// Being fetched, or waiting for its turn.
    Loading,
    Ready(ImageData),
    /// Not available (deleted, no access, or the network failed). Asking again after the
    /// connection comes back tries again.
    Failed,
}

/// A file of a sent message that is still uploading, as the journal records it so a restart
/// can finish the upload. The copy in the outbox is named by `id`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JournalAttachment {
    pub id: AttachmentId,
    pub name: String,
    pub size: u64,
}

/// A guess at a file's media type from its name, for choosing an icon. The server decides the
/// real type from the bytes.
pub fn guess_media_type(name: &str) -> &'static str {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "json" => "application/json",
        "txt" | "log" | "md" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => lazy_domain::attachment::OCTET_STREAM,
    }
}

/// The most bytes a file may have, for messages.
pub fn size_limit_text() -> String {
    format!("{} MiB", MAX_ATTACHMENT_BYTES / (1024 * 1024))
}
