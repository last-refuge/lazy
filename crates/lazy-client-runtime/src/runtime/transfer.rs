//! Moving files (slice 3A, D17): copying chosen files into the outbox, uploading them, saving
//! downloads, and fetching images for the screen.
//!
//! Every transfer is a task with a cancellation token the core can pull (`CancelUpload`,
//! `CancelDownload`, `ReleaseFiles`). The core repeats an upload whose outcome is unknown, so each
//! `UploadFile` makes exactly one attempt; reads (downloads, images) are repeated here.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lazy_client_core::{FileSource, ImageData, Input, TransferError};
use lazy_domain::emoji::MAX_EMOJI_BYTES;
use lazy_domain::{AttachmentId, ConversationId, EmojiId, EmojiName, UserId, WorkspaceId};
use lazy_protocol::{Attachment, AttachmentResponse, CustomEmojiResponse, paths};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{Inner, RequestError};
use crate::api::describe;
use crate::blob::{BlobKey, BlobReader};
use crate::images::{ImageCache, ImageKey};
use crate::outbox::{Outbox, describe_io};

/// How often upload and download progress is reported at most.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);
/// The least time an upload may take, and the rate it is assumed to proceed at beyond that.
const UPLOAD_BASE_TIMEOUT: Duration = Duration::from_secs(30);
const UPLOAD_MIN_RATE: u64 = 16 * 1024;
const DOWNLOAD_BASE_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_MIN_RATE: u64 = 16 * 1024;
/// Largest thumbnail accepted, and largest whole image.
const MAX_THUMBNAIL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = lazy_domain::attachment::MAX_ATTACHMENT_BYTES + 1024 * 1024;
/// The disk the cache of custom emoji images may use, and the largest image accepted for one.
const EMOJI_CACHE_LIMIT: u64 = 16 * 1024 * 1024;
const MAX_EMOJI_IMAGE_BYTES: u64 = 2 * 1024 * 1024;
/// The same for the avatars of people (each a 256 by 256 PNG).
const AVATAR_CACHE_LIMIT: u64 = 32 * 1024 * 1024;
const MAX_AVATAR_IMAGE_BYTES: u64 = 2 * 1024 * 1024;

/// Work for the thread that copies files into the outbox, one job at a time, in order.
enum Job {
    Stage {
        attachment: AttachmentId,
        generation: u64,
        source: FileSource,
        cancel: Arc<AtomicBool>,
    },
    Release(Vec<AttachmentId>),
    Clear,
}

/// The outbox and the image cache of a profile, once the state key is known.
pub(super) struct FileServices {
    outbox: Arc<Outbox>,
    images: Arc<ImageCache>,
    /// The images of custom emoji, 16 MiB at most.
    emoji: Arc<ImageCache>,
    /// The avatars of people, 32 MiB at most, and the versions of each fetched by this run (so a
    /// superseded one can be deleted; older ones are left to the least-recently-used order).
    avatars: Arc<ImageCache>,
    avatar_versions: Mutex<HashMap<UserId, HashSet<u32>>>,
    jobs: Mutex<std_mpsc::Sender<Job>>,
}

/// The transfers in flight, so they can be cancelled.
#[derive(Default)]
pub(super) struct Transfers {
    /// The attempt (the core's `generation`) and the token that stops it.
    uploads: HashMap<AttachmentId, (u64, CancellationToken)>,
    downloads: HashMap<AttachmentId, CancellationToken>,
    staging: HashMap<AttachmentId, Arc<AtomicBool>>,
}

impl FileServices {
    /// Opens the outbox and the image cache of `profile_dir` and starts the copying thread.
    /// Copies the journal does not name are deleted. Blocking.
    pub(super) fn open(
        profile_dir: &Path,
        key: &[u8; 32],
        keep: Option<&HashSet<AttachmentId>>,
        inputs: async_channel::Sender<Input>,
        transfers: Arc<Mutex<Transfers>>,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        let blob_key = Arc::new(BlobKey::new(key));
        let outbox = Arc::new(Outbox::new(profile_dir, blob_key.clone()));
        if let Some(keep) = keep {
            outbox.sweep(keep);
        }
        let emoji = Arc::new(ImageCache::open_named(
            profile_dir,
            "emoji",
            blob_key.clone(),
            EMOJI_CACHE_LIMIT,
        ));
        let avatars = Arc::new(ImageCache::open_named(
            profile_dir,
            "avatars",
            blob_key.clone(),
            AVATAR_CACHE_LIMIT,
        ));
        let images = Arc::new(ImageCache::open(profile_dir, blob_key));
        let (jobs, receiver) = std_mpsc::channel();
        let worker = outbox.clone();
        let spawned = std::thread::Builder::new()
            .name("lazy-outbox".to_owned())
            .spawn(move || copy_loop(receiver, worker, inputs, transfers, shutdown));
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start the thread that copies files");
        }
        Arc::new(Self {
            outbox,
            images,
            emoji,
            avatars,
            avatar_versions: Mutex::new(HashMap::new()),
            jobs: Mutex::new(jobs),
        })
    }

    fn send(&self, job: Job) {
        let _ = self
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(job);
    }
}

/// Copies files into the outbox, deletes released ones, in the order asked.
fn copy_loop(
    receiver: std_mpsc::Receiver<Job>,
    outbox: Arc<Outbox>,
    inputs: async_channel::Sender<Input>,
    transfers: Arc<Mutex<Transfers>>,
    shutdown: CancellationToken,
) {
    loop {
        match receiver.recv_timeout(Duration::from_millis(250)) {
            Ok(Job::Stage {
                attachment,
                generation,
                source,
                cancel,
            }) => {
                let result = outbox.stage(attachment, &source, &|| {
                    cancel.load(Ordering::SeqCst) || shutdown.is_cancelled()
                });
                {
                    let mut transfers = transfers.lock().unwrap_or_else(|e| e.into_inner());
                    if transfers
                        .staging
                        .get(&attachment)
                        .is_some_and(|flag| Arc::ptr_eq(flag, &cancel))
                    {
                        transfers.staging.remove(&attachment);
                    }
                }
                // A copy that was cancelled is no longer wanted by anyone.
                if !matches!(result, Err(TransferError::Cancelled)) {
                    let _ = inputs.send_blocking(Input::FileStaged {
                        attachment,
                        generation,
                        result,
                    });
                }
            }
            Ok(Job::Release(ids)) => {
                for id in ids {
                    outbox.remove(id);
                }
            }
            Ok(Job::Clear) => outbox.clear(),
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                if shutdown.is_cancelled() {
                    return;
                }
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

impl Inner {
    pub(super) fn file_services(&self) -> Option<Arc<FileServices>> {
        self.files.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    // ----- Staging -----

    pub(super) fn stage_file(
        self: &Arc<Self>,
        attachment: AttachmentId,
        generation: u64,
        source: FileSource,
    ) {
        let Some(services) = self.file_services() else {
            let inner = self.clone();
            self.spawn(async move {
                let error = TransferError::Disk(
                    "secure storage isn't available, so a safe copy can't be kept".to_owned(),
                );
                inner
                    .emit(Input::FileStaged {
                        attachment,
                        generation,
                        result: Err(error),
                    })
                    .await;
            });
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some(old) = self
            .transfers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .staging
            .insert(attachment, cancel.clone())
        {
            old.store(true, Ordering::SeqCst);
        }
        services.send(Job::Stage {
            attachment,
            generation,
            source,
            cancel,
        });
    }

    /// Stops and deletes the copies of these files, and any upload of them.
    pub(super) fn release_files(self: &Arc<Self>, attachments: Vec<AttachmentId>) {
        {
            let mut transfers = self.transfers.lock().unwrap_or_else(|e| e.into_inner());
            for id in &attachments {
                if let Some(flag) = transfers.staging.remove(id) {
                    flag.store(true, Ordering::SeqCst);
                }
                if let Some((_, token)) = transfers.uploads.remove(id) {
                    token.cancel();
                }
            }
        }
        if let Some(services) = self.file_services() {
            services.send(Job::Release(attachments));
        }
    }

    /// Cancels every transfer and deletes the outbox and the image cache (sign-out).
    pub(super) async fn forget_files(self: &Arc<Self>) {
        {
            let mut transfers = self.transfers.lock().unwrap_or_else(|e| e.into_inner());
            for (_, flag) in transfers.staging.drain() {
                flag.store(true, Ordering::SeqCst);
            }
            for (_, (_, token)) in transfers.uploads.drain() {
                token.cancel();
            }
            for (_, token) in transfers.downloads.drain() {
                token.cancel();
            }
        }
        if let Some(services) = self.file_services() {
            services.send(Job::Clear);
            let images = services.images.clone();
            let emoji = services.emoji.clone();
            let avatars = services.avatars.clone();
            services
                .avatar_versions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            let _ = tokio::task::spawn_blocking(move || {
                images.clear();
                emoji.clear();
                avatars.clear();
            })
            .await;
        }
    }

    // ----- Uploads -----

    pub(super) fn upload_file(
        self: &Arc<Self>,
        attachment: AttachmentId,
        conversation: ConversationId,
        generation: u64,
        name: String,
        media_type: String,
    ) {
        let token = self.cancel.child_token();
        if let Some(old) = self
            .transfers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .uploads
            .insert(attachment, (generation, token.clone()))
        {
            old.1.cancel();
        }
        let inner = self.clone();
        self.spawn(async move {
            let result = tokio::select! {
                () = token.cancelled() => None,
                result = inner.run_upload(attachment, conversation, generation, &name, &media_type) => Some(result),
            };
            {
                let mut transfers = inner.transfers.lock().unwrap_or_else(|e| e.into_inner());
                if transfers
                    .uploads
                    .get(&attachment)
                    .is_some_and(|(current, _)| *current == generation)
                {
                    transfers.uploads.remove(&attachment);
                }
            }
            if let Some(result) = result {
                if let Err(error) = &result {
                    tracing::warn!(reason = error.code(), "an attachment upload failed");
                }
                inner
                    .emit(Input::UploadResult {
                        attachment,
                        generation,
                        result,
                    })
                    .await;
            }
        });
    }

    pub(super) fn cancel_upload(&self, attachment: AttachmentId) {
        if let Some(token) = self
            .transfers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .uploads
            .remove(&attachment)
        {
            token.1.cancel();
        }
    }

    async fn run_upload(
        &self,
        attachment: AttachmentId,
        conversation: ConversationId,
        generation: u64,
        name: &str,
        media_type: &str,
    ) -> Result<AttachmentResponse, TransferError> {
        let Some(services) = self.file_services() else {
            return Err(TransferError::Missing);
        };
        let outbox = services.outbox.clone();
        let (reader, length) = tokio::task::spawn_blocking(move || outbox.open(attachment))
            .await
            .map_err(|e| TransferError::Unreadable(e.to_string()))??;

        // A thread reads and decrypts the copy a chunk at a time, ahead of the connection.
        let (chunks, receiver) = mpsc::channel::<io::Result<Vec<u8>>>(4);
        let local_error = Arc::new(Mutex::new(None::<String>));
        let reported = local_error.clone();
        tokio::task::spawn_blocking(move || read_chunks(reader, chunks, reported));
        let progress = Progress {
            inputs: self.inputs.clone(),
            attachment,
            generation,
            total: length,
            last: Instant::now(),
        };
        let stream = futures_util::stream::unfold(
            (receiver, 0u64, progress),
            |(mut receiver, sent, mut progress)| async move {
                let item = receiver.recv().await?;
                match item {
                    Ok(chunk) => {
                        let sent = sent + chunk.len() as u64;
                        progress.report(sent);
                        Some((Ok(chunk), (receiver, sent, progress)))
                    }
                    Err(error) => Some((Err(error), (receiver, sent, progress))),
                }
            },
        );
        let timeout = UPLOAD_BASE_TIMEOUT + Duration::from_secs(length / UPLOAD_MIN_RATE);
        let result = self
            .api
            .put_attachment(
                &paths::attachment_upload(conversation, attachment),
                name,
                media_type,
                length,
                reqwest::Body::wrap_stream(stream),
                timeout,
            )
            .await;
        let local = local_error.lock().unwrap_or_else(|e| e.into_inner()).take();
        match (result, local) {
            (Ok(response), _) => Ok(response),
            // The copy could not be read: that, not the connection, is what went wrong.
            (Err(RequestError::Unknown(_)), Some(why)) => Err(TransferError::Unreadable(why)),
            (Err(error), _) => Err(TransferError::Request(error)),
        }
    }

    // ----- Downloads -----

    pub(super) fn download_attachment(
        self: &Arc<Self>,
        attachment: Attachment,
        destination: PathBuf,
    ) {
        let id = attachment.id;
        let token = self.cancel.child_token();
        if let Some(old) = self
            .transfers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .downloads
            .insert(id, token.clone())
        {
            old.cancel();
        }
        let inner = self.clone();
        self.spawn(async move {
            let result = tokio::select! {
                () = token.cancelled() => None,
                result = inner.run_download(&attachment, &destination) => Some(result),
            };
            if let Some(result) = result {
                inner
                    .transfers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .downloads
                    .remove(&id);
                inner
                    .emit(Input::DownloadResult {
                        attachment: id,
                        result,
                    })
                    .await;
            }
        });
    }

    pub(super) fn cancel_download(&self, attachment: AttachmentId) {
        if let Some(token) = self
            .transfers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .downloads
            .remove(&attachment)
        {
            token.cancel();
        }
    }

    async fn run_download(
        &self,
        attachment: &Attachment,
        destination: &Path,
    ) -> Result<PathBuf, TransferError> {
        let timeout =
            DOWNLOAD_BASE_TIMEOUT + Duration::from_secs(attachment.size / DOWNLOAD_MIN_RATE);
        let mut response = self
            .api
            .get_response(&paths::attachment(attachment.id), timeout)
            .await
            .map_err(TransferError::Request)?;
        let directory = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let temp = directory.join(format!(".lazy-{}.part", attachment.id));
        // Whatever happens next, the temporary file does not outlive this call.
        let mut guard = TempFile(Some(temp.clone()));

        let (chunks, mut receiver) = mpsc::channel::<Vec<u8>>(8);
        let writer_path = temp.clone();
        let writer = tokio::task::spawn_blocking(move || -> io::Result<(String, u64)> {
            let mut file = std::fs::File::create(&writer_path)?;
            let mut hasher = Sha256::new();
            let mut written = 0u64;
            while let Some(chunk) = receiver.blocking_recv() {
                file.write_all(&chunk)?;
                hasher.update(&chunk);
                written += chunk.len() as u64;
            }
            file.sync_all()?;
            Ok((hex(&hasher.finalize()), written))
        });

        let mut received = 0u64;
        let mut last = Instant::now();
        let outcome = loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    received += chunk.len() as u64;
                    if received > attachment.size {
                        break Err(TransferError::ChecksumMismatch);
                    }
                    if last.elapsed() >= PROGRESS_INTERVAL {
                        last = Instant::now();
                        let _ = self.inputs.try_send(Input::DownloadProgress {
                            attachment: attachment.id,
                            received,
                            total: attachment.size,
                        });
                    }
                    if chunks.send(chunk.to_vec()).await.is_err() {
                        // The writer stopped; its error is below.
                        break Ok(());
                    }
                }
                Ok(None) => break Ok(()),
                Err(error) => {
                    break Err(TransferError::Request(RequestError::Unknown(describe(
                        &error,
                    ))));
                }
            }
        };
        drop(chunks);
        let written = writer.await;
        outcome?;
        let (digest, size) = match written {
            Ok(Ok(done)) => done,
            Ok(Err(error)) => return Err(TransferError::Disk(describe_io(&error))),
            Err(error) => return Err(TransferError::Disk(error.to_string())),
        };
        if size != attachment.size || !digest.eq_ignore_ascii_case(&attachment.sha256) {
            return Err(TransferError::ChecksumMismatch);
        }
        let from = temp;
        let to = destination.to_owned();
        let renamed = tokio::task::spawn_blocking(move || std::fs::rename(&from, &to))
            .await
            .map_err(|e| TransferError::Disk(e.to_string()))?;
        renamed.map_err(|e| TransferError::Disk(describe_io(&e)))?;
        guard.0 = None;
        Ok(destination.to_owned())
    }

    // ----- Images -----

    pub(super) fn fetch_image(self: &Arc<Self>, attachment: AttachmentId, full: bool) {
        let inner = self.clone();
        self.spawn(async move {
            let result = inner.run_image(attachment, full).await;
            inner
                .emit(Input::ImageLoaded {
                    attachment,
                    full,
                    result,
                })
                .await;
        });
    }

    async fn run_image(
        &self,
        attachment: AttachmentId,
        full: bool,
    ) -> Result<ImageData, TransferError> {
        let services = self.file_services();
        let image = ImageKey { attachment, full };
        if let Some(services) = &services {
            let images = services.images.clone();
            let cached = tokio::task::spawn_blocking(move || images.get(image))
                .await
                .ok()
                .flatten();
            if let Some(data) = cached {
                return Ok(data);
            }
        }
        let (path, max) = if full {
            (paths::attachment(attachment), MAX_IMAGE_BYTES)
        } else {
            (paths::attachment_thumbnail(attachment), MAX_THUMBNAIL_BYTES)
        };
        let timeout = DOWNLOAD_BASE_TIMEOUT + Duration::from_secs(max / DOWNLOAD_MIN_RATE);
        let bytes = self
            .retry_read(|| self.api.get_bytes(&path, max, timeout))
            .await
            .map_err(TransferError::Request)?;
        let data = ImageData::new(bytes);
        if let Some(services) = services {
            let images = services.images.clone();
            let kept = data.clone();
            let _ = tokio::task::spawn_blocking(move || images.put(image, &kept)).await;
        }
        Ok(data)
    }

    // ----- Custom emoji -----

    pub(super) fn upload_emoji(
        self: &Arc<Self>,
        workspace: WorkspaceId,
        emoji: EmojiId,
        name: EmojiName,
        source: FileSource,
    ) {
        let inner = self.clone();
        self.spawn(async move {
            let result = inner
                .run_emoji_upload(workspace, emoji, &name, source)
                .await;
            inner
                .emit(Input::CustomEmojiUploaded { emoji, result })
                .await;
        });
    }

    async fn run_emoji_upload(
        &self,
        workspace: WorkspaceId,
        emoji: EmojiId,
        name: &EmojiName,
        source: FileSource,
    ) -> Result<CustomEmojiResponse, TransferError> {
        let bytes = match source {
            FileSource::Bytes(bytes) => {
                if bytes.len() as u64 > MAX_EMOJI_BYTES {
                    return Err(TransferError::TooLarge {
                        size: bytes.len() as u64,
                    });
                }
                bytes.as_slice().to_vec()
            }
            FileSource::Path(path) => tokio::task::spawn_blocking(move || read_emoji_file(&path))
                .await
                .map_err(|e| TransferError::Unreadable(e.to_string()))??,
        };
        let media_type = lazy_domain::attachment::sniff_media_type(&bytes)
            .unwrap_or(lazy_domain::attachment::OCTET_STREAM);
        self.api
            .put_emoji(
                &paths::emoji_upload(workspace, emoji),
                name.as_str(),
                media_type,
                bytes,
                Duration::from_secs(60),
            )
            .await
            .map_err(TransferError::Request)
    }

    pub(super) fn fetch_emoji_image(self: &Arc<Self>, emoji: EmojiId) {
        let inner = self.clone();
        self.spawn(async move {
            let result = inner.run_emoji_image(emoji).await;
            inner.emit(Input::EmojiImageLoaded { emoji, result }).await;
        });
    }

    async fn run_emoji_image(&self, emoji: EmojiId) -> Result<ImageData, TransferError> {
        let services = self.file_services();
        // The emoji cache is a cache of its own, so an emoji's id is the key an attachment's is.
        let key = ImageKey {
            attachment: AttachmentId::from_uuid(emoji.as_uuid()),
            full: false,
        };
        if let Some(services) = &services {
            let cache = services.emoji.clone();
            let cached = tokio::task::spawn_blocking(move || cache.get(key))
                .await
                .ok()
                .flatten();
            if let Some(data) = cached {
                return Ok(data);
            }
        }
        let path = paths::emoji(emoji);
        let timeout = DOWNLOAD_BASE_TIMEOUT;
        let bytes = self
            .retry_read(|| self.api.get_bytes(&path, MAX_EMOJI_IMAGE_BYTES, timeout))
            .await
            .map_err(TransferError::Request)?;
        let data = ImageData::new(bytes);
        if let Some(services) = services {
            let cache = services.emoji.clone();
            let kept = data.clone();
            let _ = tokio::task::spawn_blocking(move || cache.put(key, &kept)).await;
        }
        Ok(data)
    }

    pub(super) fn forget_emoji_images(self: &Arc<Self>, emojis: Vec<EmojiId>) {
        let Some(services) = self.file_services() else {
            return;
        };
        let cache = services.emoji.clone();
        self.spawn_unguarded(async move {
            let ids: Vec<AttachmentId> = emojis
                .iter()
                .map(|emoji| AttachmentId::from_uuid(emoji.as_uuid()))
                .collect();
            let _ = tokio::task::spawn_blocking(move || cache.forget(&ids)).await;
        });
    }

    // ----- Avatars -----

    /// Reads and uploads the picture the person chose. Answered by an `ActionResult`.
    pub(super) fn set_avatar(self: &Arc<Self>, source: FileSource) {
        let inner = self.clone();
        self.spawn(async move {
            let result = inner.run_avatar_upload(source).await;
            let kind = lazy_client_core::ActionKind::SetAvatar;
            inner.emit(Input::ActionResult { kind, result }).await;
        });
    }

    async fn run_avatar_upload(
        &self,
        source: FileSource,
    ) -> Result<lazy_client_core::ActionOutcome, RequestError> {
        use lazy_domain::workspace::MAX_AVATAR_UPLOAD_BYTES;
        let refuse = |code: lazy_protocol::ErrorCode, message: String| {
            RequestError::Rejected(lazy_protocol::ApiError::new(code, message))
        };
        let too_large = |size: u64| {
            refuse(
                lazy_protocol::ErrorCode::PayloadTooLarge,
                format!(
                    "That picture is {}; a picture can be at most {} MiB.",
                    format_size(size),
                    MAX_AVATAR_UPLOAD_BYTES / (1024 * 1024)
                ),
            )
        };
        let bytes = match source {
            FileSource::Bytes(bytes) => {
                if bytes.len() > MAX_AVATAR_UPLOAD_BYTES {
                    return Err(too_large(bytes.len() as u64));
                }
                bytes.as_slice().to_vec()
            }
            FileSource::Path(path) => {
                match tokio::task::spawn_blocking(move || read_avatar_file(&path)).await {
                    Ok(Ok(bytes)) => bytes,
                    Ok(Err(TransferError::TooLarge { size })) => return Err(too_large(size)),
                    Ok(Err(TransferError::Unreadable(reason) | TransferError::Disk(reason))) => {
                        return Err(refuse(
                            lazy_protocol::ErrorCode::InvalidRequest,
                            format!("Couldn't read that file: {reason}."),
                        ));
                    }
                    Ok(Err(_)) => {
                        return Err(refuse(
                            lazy_protocol::ErrorCode::InvalidRequest,
                            "Couldn't read that file.".to_owned(),
                        ));
                    }
                    Err(error) => {
                        return Err(refuse(
                            lazy_protocol::ErrorCode::InvalidRequest,
                            format!("Couldn't read that file: {error}."),
                        ));
                    }
                }
            }
        };
        // The type is only a hint; the server looks at the bytes.
        let media_type = lazy_domain::attachment::sniff_media_type(&bytes)
            .unwrap_or(lazy_domain::attachment::OCTET_STREAM);
        let response = self
            .api
            .put_avatar(
                paths::ME_AVATAR_ROUTE,
                media_type,
                bytes,
                Duration::from_secs(60),
            )
            .await?;
        Ok(lazy_client_core::ActionOutcome::Profile(response.user))
    }

    pub(super) fn fetch_avatar(self: &Arc<Self>, user: UserId, version: u32) {
        let inner = self.clone();
        self.spawn(async move {
            let result = inner.run_avatar_fetch(user, version).await;
            inner
                .emit(Input::AvatarLoaded {
                    user,
                    version,
                    result,
                })
                .await;
        });
    }

    async fn run_avatar_fetch(
        &self,
        user: UserId,
        version: u32,
    ) -> Result<ImageData, TransferError> {
        let services = self.file_services();
        let key = avatar_key(user, version);
        if let Some(services) = &services {
            let cache = services.avatars.clone();
            let cached = tokio::task::spawn_blocking(move || cache.get(key))
                .await
                .ok()
                .flatten();
            if let Some(data) = cached {
                return Ok(data);
            }
        }
        let path = paths::user_avatar(user, version);
        let bytes = self
            .retry_read(|| {
                self.api
                    .get_bytes(&path, MAX_AVATAR_IMAGE_BYTES, DOWNLOAD_BASE_TIMEOUT)
            })
            .await
            .map_err(TransferError::Request)?;
        let data = ImageData::new(bytes);
        if let Some(services) = services {
            services
                .avatar_versions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(user)
                .or_default()
                .insert(version);
            let cache = services.avatars.clone();
            let kept = data.clone();
            let _ = tokio::task::spawn_blocking(move || cache.put(key, &kept)).await;
        }
        Ok(data)
    }

    /// These people have another avatar now: the pictures this run fetched for them are dropped.
    pub(super) fn forget_avatars(self: &Arc<Self>, users: Vec<UserId>) {
        let Some(services) = self.file_services() else {
            return;
        };
        let keys: Vec<AttachmentId> = {
            let mut versions = services
                .avatar_versions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            users
                .iter()
                .flat_map(|user| {
                    versions
                        .remove(user)
                        .into_iter()
                        .flatten()
                        .map(|version| avatar_key(*user, version).attachment)
                        .collect::<Vec<_>>()
                })
                .collect()
        };
        if keys.is_empty() {
            return;
        }
        let cache = services.avatars.clone();
        self.spawn_unguarded(async move {
            let _ = tokio::task::spawn_blocking(move || cache.forget(&keys)).await;
        });
    }

    pub(super) fn forget_attachments(self: &Arc<Self>, attachments: Vec<AttachmentId>) {
        let Some(services) = self.file_services() else {
            return;
        };
        let images = services.images.clone();
        self.spawn_unguarded(async move {
            let _ = tokio::task::spawn_blocking(move || images.forget(&attachments)).await;
        });
    }
}

/// Reads the image of a new custom emoji, refusing one over the limit before reading it.
fn read_emoji_file(path: &Path) -> Result<Vec<u8>, TransferError> {
    use std::io::Read;
    let unreadable = |error: io::Error| TransferError::Unreadable(describe_io(&error));
    let metadata = std::fs::metadata(path).map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(TransferError::Unreadable("it is not a file".to_owned()));
    }
    if metadata.len() > MAX_EMOJI_BYTES {
        return Err(TransferError::TooLarge {
            size: metadata.len(),
        });
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(unreadable)?
        .take(MAX_EMOJI_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() as u64 > MAX_EMOJI_BYTES {
        return Err(TransferError::TooLarge {
            size: bytes.len() as u64,
        });
    }
    Ok(bytes)
}

/// Reads a copy a chunk at a time into `chunks`. A copy that cannot be read ends the stream with
/// an error and leaves the reason in `reported`.
fn read_chunks<R: io::Read>(
    mut reader: BlobReader<R>,
    chunks: mpsc::Sender<io::Result<Vec<u8>>>,
    reported: Arc<Mutex<Option<String>>>,
) {
    loop {
        match reader.next_chunk() {
            Ok(Some(chunk)) => {
                if chunks.blocking_send(Ok(chunk)).is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(error) => {
                *reported.lock().unwrap_or_else(|e| e.into_inner()) = Some(describe_io(&error));
                let _ = chunks.blocking_send(Err(error));
                return;
            }
        }
    }
}

/// Throttled upload progress.
struct Progress {
    inputs: async_channel::Sender<Input>,
    attachment: AttachmentId,
    generation: u64,
    total: u64,
    last: Instant,
}

impl Progress {
    fn report(&mut self, sent: u64) {
        if self.last.elapsed() < PROGRESS_INTERVAL {
            return;
        }
        self.last = Instant::now();
        // Progress is a courtesy: if the host is busy it skips this one.
        let _ = self.inputs.try_send(Input::UploadProgress {
            attachment: self.attachment,
            generation: self.generation,
            sent,
            total: self.total,
        });
    }
}

/// A temporary file that is deleted when this goes away, unless it was taken.
struct TempFile(Option<PathBuf>);

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The cache key of a person's avatar at a version: a hash, so no two (person, version) pairs
/// share a file and the file's name says nothing about who it is of.
fn avatar_key(user: UserId, version: u32) -> ImageKey {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"lazy-avatar");
    hasher.update(user.as_uuid().as_bytes());
    hasher.update(version.to_le_bytes());
    let digest = hasher.finalize();
    // The first 16 bytes of the hash, written as the id they stand in for.
    let hex: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let text = format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    let attachment = text
        .parse::<AttachmentId>()
        .unwrap_or_else(|_| AttachmentId::from_uuid(user.as_uuid()));
    ImageKey {
        attachment,
        full: false,
    }
}

/// Reads the picture chosen as an avatar, refusing one over the limit before reading it.
fn read_avatar_file(path: &Path) -> Result<Vec<u8>, TransferError> {
    use lazy_domain::workspace::MAX_AVATAR_UPLOAD_BYTES;
    use std::io::Read;
    let limit = MAX_AVATAR_UPLOAD_BYTES as u64;
    let unreadable = |error: io::Error| TransferError::Unreadable(describe_io(&error));
    let metadata = std::fs::metadata(path).map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(TransferError::Unreadable("it is not a file".to_owned()));
    }
    if metadata.len() > limit {
        return Err(TransferError::TooLarge {
            size: metadata.len(),
        });
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(unreadable)?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() as u64 > limit {
        return Err(TransferError::TooLarge {
            size: bytes.len() as u64,
        });
    }
    Ok(bytes)
}

/// A size for a person: "6.2 MiB".
fn format_size(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}
