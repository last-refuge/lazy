//! Thumbnails and images for the screen (slice 3A, D17): a small layer in memory over an
//! encrypted cache on disk, bounded to 64 MiB, least recently used first.
//!
//! The cache is disposable. A file that cannot be read (damaged, from another key, cut off by a
//! crash) is deleted and fetched again, without a word to the person. It is emptied at sign-out
//! and when a message is deleted.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use lazy_client_core::ImageData;
use lazy_domain::AttachmentId;

use crate::blob::{BlobKey, BlobKind, BlobWriter, CHUNK, open_file};
use crate::outbox::{create_private, private_dir, sync_dir};

/// Most bytes kept on disk.
pub(crate) const DISK_LIMIT: u64 = 64 * 1024 * 1024;
/// Most bytes kept in memory.
const MEMORY_LIMIT: usize = 8 * 1024 * 1024;
/// Most images kept in memory.
const MEMORY_COUNT: usize = 64;

/// Which image of an attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ImageKey {
    pub(crate) attachment: AttachmentId,
    pub(crate) full: bool,
}

#[derive(Default)]
struct DiskIndex {
    /// Size on disk and when it was last used (a counter, larger is newer).
    entries: HashMap<ImageKey, (u64, u64)>,
    total: u64,
    clock: u64,
}

#[derive(Default)]
struct Memory {
    entries: HashMap<ImageKey, (ImageData, u64)>,
    bytes: usize,
    clock: u64,
}

pub(crate) struct ImageCache {
    dir: PathBuf,
    key: Arc<BlobKey>,
    disk_limit: u64,
    index: Mutex<DiskIndex>,
    memory: Mutex<Memory>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl ImageCache {
    /// Opens the cache in `profile_dir`, counting what is already there. Blocking.
    pub(crate) fn open(profile_dir: &Path, key: Arc<BlobKey>) -> Self {
        Self::open_with_limit(profile_dir, key, DISK_LIMIT)
    }

    pub(crate) fn open_with_limit(profile_dir: &Path, key: Arc<BlobKey>, disk_limit: u64) -> Self {
        Self::open_named(profile_dir, "images", key, disk_limit)
    }

    /// Opens a cache kept in the directory `name` of `profile_dir`. (The custom emoji of a
    /// workspace have a cache of their own; there an emoji's id stands in for an attachment's.)
    pub(crate) fn open_named(
        profile_dir: &Path,
        name: &str,
        key: Arc<BlobKey>,
        disk_limit: u64,
    ) -> Self {
        let dir = profile_dir.join(name);
        let mut found: Vec<(std::time::SystemTime, ImageKey, u64)> = Vec::new();
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                let parsed = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(parse_name);
                let metadata = entry.metadata().ok();
                match (parsed, metadata) {
                    (Some(image), Some(metadata)) => found.push((
                        metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
                        image,
                        metadata.len(),
                    )),
                    // Half-written files and strangers are not ours to keep.
                    _ => {
                        let _ = fs::remove_file(path);
                    }
                }
            }
        }
        found.sort_by_key(|(modified, _, _)| *modified);
        let mut index = DiskIndex::default();
        for (_, image, size) in found {
            index.clock += 1;
            index.total += size;
            index.entries.insert(image, (size, index.clock));
        }
        let cache = Self {
            dir,
            key,
            disk_limit,
            index: Mutex::new(index),
            memory: Mutex::new(Memory::default()),
        };
        cache.evict();
        cache
    }

    fn path(&self, image: ImageKey) -> PathBuf {
        self.dir.join(file_name(image))
    }

    /// The image, from memory or disk. A cache file that cannot be read is deleted and counts as
    /// a miss. Blocking.
    pub(crate) fn get(&self, image: ImageKey) -> Option<ImageData> {
        {
            let mut memory = lock(&self.memory);
            memory.clock += 1;
            let clock = memory.clock;
            if let Some((data, used)) = memory.entries.get_mut(&image) {
                *used = clock;
                let data = data.clone();
                drop(memory);
                self.touch(image);
                return Some(data);
            }
        }
        if !lock(&self.index).entries.contains_key(&image) {
            return None;
        }
        match self.read(image) {
            Ok(data) => {
                self.touch(image);
                self.remember(image, &data);
                Some(data)
            }
            Err(_) => {
                self.forget_one(image);
                None
            }
        }
    }

    /// Marks an image as just used, in the order kept now and, through the file's time, in the
    /// order a later start finds.
    fn touch(&self, image: ImageKey) {
        {
            let mut index = lock(&self.index);
            index.clock += 1;
            let clock = index.clock;
            if let Some(entry) = index.entries.get_mut(&image) {
                entry.1 = clock;
            }
        }
        if let Ok(file) = File::open(self.path(image)) {
            let _ = file.set_modified(std::time::SystemTime::now());
        }
    }

    fn read(&self, image: ImageKey) -> io::Result<ImageData> {
        let (mut reader, size) = open_file(&self.path(image), self.key.clone(), BlobKind::Image)?;
        let mut bytes = Vec::with_capacity(size as usize);
        while let Some(chunk) = reader.next_chunk()? {
            bytes.extend_from_slice(&chunk);
        }
        Ok(ImageData::new(bytes))
    }

    /// Keeps an image that was just fetched. A failure to write only means it is fetched again
    /// next time. Blocking.
    pub(crate) fn put(&self, image: ImageKey, data: &ImageData) {
        self.remember(image, data);
        let size_estimate = data.len() as u64;
        if size_estimate + 1024 > self.disk_limit {
            return;
        }
        let written = self.write(image, data);
        let Ok(size) = written else {
            return;
        };
        {
            let mut index = lock(&self.index);
            index.clock += 1;
            let clock = index.clock;
            if let Some((old, _)) = index.entries.insert(image, (size, clock)) {
                index.total = index.total.saturating_sub(old);
            }
            index.total += size;
        }
        self.evict();
    }

    fn write(&self, image: ImageKey, data: &ImageData) -> io::Result<u64> {
        private_dir(&self.dir)?;
        let part = self.dir.join(format!("{}.part", file_name(image)));
        let result = (|| {
            let file = create_private(&part)?;
            let mut writer =
                BlobWriter::new(BufWriter::new(file), self.key.clone(), BlobKind::Image)?;
            for piece in data.bytes().chunks(CHUNK) {
                writer.push(piece)?;
            }
            let out = writer.finish()?;
            let file: File = out.into_inner().map_err(|e| e.into_error())?;
            file.sync_all()?;
            let size = file.metadata()?.len();
            fs::rename(&part, self.path(image))?;
            Ok(size)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&part);
        } else {
            sync_dir(&self.dir);
        }
        result
    }

    fn remember(&self, image: ImageKey, data: &ImageData) {
        let mut memory = lock(&self.memory);
        if data.len() > MEMORY_LIMIT / 2 {
            return;
        }
        memory.clock += 1;
        let clock = memory.clock;
        if let Some((old, _)) = memory.entries.insert(image, (data.clone(), clock)) {
            memory.bytes -= old.len();
        }
        memory.bytes += data.len();
        while memory.bytes > MEMORY_LIMIT || memory.entries.len() > MEMORY_COUNT {
            let oldest = memory
                .entries
                .iter()
                .filter(|(key, _)| **key != image)
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| *key);
            let Some(oldest) = oldest else {
                break;
            };
            if let Some((old, _)) = memory.entries.remove(&oldest) {
                memory.bytes -= old.len();
            }
        }
    }

    /// Deletes the least recently used files until the cache fits.
    fn evict(&self) {
        let mut doomed = Vec::new();
        {
            let mut index = lock(&self.index);
            while index.total > self.disk_limit {
                let Some(oldest) = index
                    .entries
                    .iter()
                    .min_by_key(|(_, (_, used))| *used)
                    .map(|(key, _)| *key)
                else {
                    break;
                };
                if let Some((size, _)) = index.entries.remove(&oldest) {
                    index.total = index.total.saturating_sub(size);
                }
                doomed.push(oldest);
            }
        }
        for image in doomed {
            let _ = fs::remove_file(self.path(image));
            self.forget_memory(image);
        }
    }

    fn forget_memory(&self, image: ImageKey) {
        let mut memory = lock(&self.memory);
        if let Some((old, _)) = memory.entries.remove(&image) {
            memory.bytes -= old.len();
        }
    }

    fn forget_one(&self, image: ImageKey) {
        {
            let mut index = lock(&self.index);
            if let Some((size, _)) = index.entries.remove(&image) {
                index.total = index.total.saturating_sub(size);
            }
        }
        let _ = fs::remove_file(self.path(image));
        self.forget_memory(image);
    }

    /// Drops every image of these attachments (their message was deleted). Blocking.
    pub(crate) fn forget(&self, attachments: &[AttachmentId]) {
        for attachment in attachments {
            for full in [false, true] {
                self.forget_one(ImageKey {
                    attachment: *attachment,
                    full,
                });
            }
        }
    }

    /// Deletes everything (sign-out). Blocking.
    pub(crate) fn clear(&self) {
        *lock(&self.index) = DiskIndex::default();
        *lock(&self.memory) = Memory::default();
        let _ = fs::remove_dir_all(&self.dir);
    }

    #[cfg(test)]
    pub(crate) fn disk_bytes(&self) -> u64 {
        lock(&self.index).total
    }

    #[cfg(test)]
    pub(crate) fn disk_files(&self) -> usize {
        fs::read_dir(&self.dir).map_or(0, |entries| entries.count())
    }
}

fn file_name(image: ImageKey) -> String {
    format!(
        "{}-{}.bin",
        if image.full { "f" } else { "t" },
        image.attachment
    )
}

fn parse_name(name: &str) -> Option<ImageKey> {
    let stem = name.strip_suffix(".bin")?;
    let (kind, id) = stem.split_once('-')?;
    let full = match kind {
        "t" => false,
        "f" => true,
        _ => return None,
    };
    Some(ImageKey {
        attachment: id.parse().ok()?,
        full,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(dir: &Path, limit: u64) -> ImageCache {
        ImageCache::open_with_limit(dir, Arc::new(BlobKey::new(&[4; 32])), limit)
    }

    fn key(full: bool) -> ImageKey {
        ImageKey {
            attachment: AttachmentId::generate(),
            full,
        }
    }

    fn data(len: usize, fill: u8) -> ImageData {
        ImageData::new(vec![fill; len])
    }

    #[test]
    fn an_image_comes_back_from_memory_and_from_disk_after_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let image = key(false);
        let first = cache(dir.path(), DISK_LIMIT);
        assert!(first.get(image).is_none());
        first.put(image, &data(1000, 7));
        assert_eq!(first.get(image), Some(data(1000, 7)));
        // A new process finds it on disk, encrypted.
        let second = cache(dir.path(), DISK_LIMIT);
        assert_eq!(second.get(image), Some(data(1000, 7)));
        let on_disk = fs::read(second.path(image)).unwrap();
        assert!(!on_disk.windows(16).any(|w| w == [7u8; 16]));
        // The thumbnail and the full image of one attachment are different entries.
        let full = ImageKey {
            full: true,
            ..image
        };
        assert!(second.get(full).is_none());
    }

    #[test]
    fn the_disk_stays_within_its_bound_and_drops_the_least_recently_used() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path(), 4000);
        let (a, b, c) = (key(false), key(false), key(false));
        cache.put(a, &data(1500, 1));
        cache.put(b, &data(1500, 2));
        // Using `a` makes `b` the oldest.
        let _ = cache.get(a);
        cache.put(c, &data(1500, 3));
        assert!(cache.disk_bytes() <= 4000);
        // Look at the files, not the memory layer.
        let reopened =
            super::ImageCache::open_with_limit(dir.path(), Arc::new(BlobKey::new(&[4; 32])), 4000);
        assert!(reopened.get(a).is_some());
        assert!(reopened.get(c).is_some());
        assert!(reopened.get(b).is_none());
    }

    #[test]
    fn a_startup_over_the_bound_trims_the_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let big = cache(dir.path(), DISK_LIMIT);
        for n in 0..4 {
            big.put(key(false), &data(2000, n));
        }
        assert_eq!(big.disk_files(), 4);
        let small = cache(dir.path(), 5000);
        assert!(small.disk_bytes() <= 5000);
        assert!(small.disk_files() < 4);
    }

    #[test]
    fn a_cache_of_its_own_directory_keeps_its_own_bound_apart_from_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let key = Arc::new(BlobKey::new(&[4; 32]));
        let emoji = ImageCache::open_named(dir.path(), "emoji", key.clone(), 4000);
        let images = ImageCache::open(dir.path(), key);
        let a = AttachmentId::generate();
        let one = ImageKey {
            attachment: a,
            full: false,
        };
        // The same key in two caches is two files.
        emoji.put(one, &data(1000, 1));
        images.put(one, &data(2000, 2));
        assert_eq!(emoji.get(one), Some(data(1000, 1)));
        assert_eq!(images.get(one), Some(data(2000, 2)));
        assert!(dir.path().join("emoji").is_dir() && dir.path().join("images").is_dir());
        // Filling one past its bound does not touch the other.
        for _ in 0..5 {
            emoji.put(key_for(), &data(1500, 3));
        }
        assert!(emoji.disk_bytes() <= 4000);
        assert_eq!(images.get(one), Some(data(2000, 2)));
        emoji.clear();
        assert!(!dir.path().join("emoji").exists());
        assert!(dir.path().join("images").exists());
    }

    fn key_for() -> ImageKey {
        key(false)
    }

    #[test]
    fn an_image_too_big_for_the_disk_bound_is_not_cached_there() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path(), 1000);
        let image = key(true);
        cache.put(image, &data(5000, 1));
        assert_eq!(cache.disk_files(), 0);
        assert_eq!(cache.disk_bytes(), 0);
    }

    #[test]
    fn a_damaged_file_is_dropped_without_a_fuss_and_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let first = cache(dir.path(), DISK_LIMIT);
        let image = key(false);
        first.put(image, &data(3000, 9));
        let path = first.path(image);
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&path, bytes).unwrap();
        // A fresh process, so the memory layer cannot answer.
        let second = cache(dir.path(), DISK_LIMIT);
        assert!(second.get(image).is_none());
        assert!(!path.exists());
        assert_eq!(second.disk_bytes(), 0);
        // And a file cut short, or from another key.
        second.put(image, &data(10, 1));
        fs::write(second.path(image), b"short").unwrap();
        let third = cache(dir.path(), DISK_LIMIT);
        assert!(third.get(image).is_none());
        let other_key =
            ImageCache::open_with_limit(dir.path(), Arc::new(BlobKey::new(&[1; 32])), DISK_LIMIT);
        let again = key(false);
        third.put(again, &data(10, 2));
        let reopened =
            ImageCache::open_with_limit(dir.path(), Arc::new(BlobKey::new(&[1; 32])), DISK_LIMIT);
        assert!(reopened.get(again).is_none());
        let _ = other_key;
    }

    #[test]
    fn forgetting_and_clearing_remove_files_and_memory() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path(), DISK_LIMIT);
        let image = key(false);
        let full = ImageKey {
            full: true,
            ..image
        };
        cache.put(image, &data(10, 1));
        cache.put(full, &data(20, 2));
        cache.forget(&[image.attachment]);
        assert!(cache.get(image).is_none());
        assert!(cache.get(full).is_none());
        assert_eq!(cache.disk_files(), 0);
        cache.put(image, &data(10, 1));
        cache.clear();
        assert!(cache.get(image).is_none());
        assert!(!dir.path().join("images").exists());
    }

    #[test]
    fn strangers_and_half_written_files_are_swept_at_open() {
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("images");
        fs::create_dir_all(&images).unwrap();
        fs::write(images.join("junk.txt"), b"x").unwrap();
        fs::write(
            images.join(format!("t-{}.bin.part", AttachmentId::generate())),
            b"x",
        )
        .unwrap();
        let _ = cache(dir.path(), DISK_LIMIT);
        assert_eq!(fs::read_dir(&images).unwrap().count(), 0);
    }
}
