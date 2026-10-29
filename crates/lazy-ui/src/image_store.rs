//! Decoded images for the views: thumbnails in message rows, the full image in the viewer, and
//! the small previews in the composer's tray.
//!
//! Decoding happens on a background thread and never in `render`. A decoded image is a
//! [`RenderImage`] of exactly one frame: a GIF shows its first frame and does not animate, and an
//! image larger than the cap for its use is scaled down while it is decoded, so a 60-megapixel
//! photo never becomes a 240 MB texture. The store keeps a bounded number of decoded pixels and
//! forgets the least recently used when it is over, so scrolling through a long conversation of
//! photos holds memory steady; a forgotten image is decoded again if its row comes back.

use std::cell::Cell;
use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext as _, Context, Entity, Global, RenderImage};
use image::{Frame, GenericImageView as _, ImageReader};
use lazy_client_core::{FileBytes, ImageData};
use lazy_domain::attachment::MAX_IMAGE_PIXELS;
use lazy_domain::{AttachmentId, EmojiId, UserId};

/// The long side, in pixels, a message thumbnail is decoded to at most (the server's own limit).
pub const THUMBNAIL_EDGE: u32 = 640;
/// The long side a full image is decoded to at most: large enough for any window, small enough
/// for every GPU's texture limit.
pub const FULL_EDGE: u32 = 4096;
/// The long side of a custom emoji's picture (the server's own limit).
pub const EMOJI_EDGE: u32 = 128;
/// The side of a person's avatar, which the server stores as exactly this square.
pub const AVATAR_EDGE: u32 = 256;
/// The long side of a tray chip's preview.
pub const CHIP_EDGE: u32 = 96;
/// Decoded pixel memory the store keeps, in bytes (RGBA).
pub const BUDGET_BYTES: u64 = 192 * 1024 * 1024;

/// What an image in the store is for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ImageKey {
    /// A message's thumbnail.
    Thumbnail(AttachmentId),
    /// A message's image at full size, for the viewer.
    Full(AttachmentId),
    /// A file chosen for the composer, by the id the composer gave it.
    Chip(String),
    /// A custom emoji's picture.
    Emoji(EmojiId),
    /// A person's avatar at one of its versions.
    Avatar(UserId, u32),
}

/// Where the bytes of an image are.
#[derive(Debug, Clone)]
pub enum Source {
    Memory(Arc<[u8]>),
    /// A thumbnail or image the core fetched.
    Data(ImageData),
    /// Bytes the person pasted.
    Pasted(FileBytes),
    File(PathBuf),
}

/// An image that could not be shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// More pixels than [`MAX_IMAGE_PIXELS`].
    TooLarge,
    /// The file could not be read.
    Unreadable(String),
    /// The bytes are not an image this client can decode.
    Unsupported(String),
}

impl DecodeError {
    /// Words for a person.
    pub fn text(&self) -> String {
        match self {
            Self::TooLarge => "This image is too large to preview.".to_owned(),
            Self::Unreadable(_) => "The file could not be read.".to_owned(),
            Self::Unsupported(_) => "This image could not be shown.".to_owned(),
        }
    }
}

/// A decoded image and its size in pixels (after any scaling down).
#[derive(Clone)]
pub struct Decoded {
    pub image: Arc<RenderImage>,
    pub width: u32,
    pub height: u32,
}

/// Decodes the first frame of `bytes`, scaled down so its long side is at most `max_edge`.
///
/// Pure and blocking: run it on a background thread.
pub fn decode(bytes: &[u8], max_edge: u32) -> Result<Decoded, DecodeError> {
    let (width, height) = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| DecodeError::Unreadable(error.to_string()))?
        .into_dimensions()
        .map_err(|error| DecodeError::Unsupported(error.to_string()))?;
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return Err(DecodeError::TooLarge);
    }
    // For a GIF this is the first frame.
    let mut picture = image::load_from_memory(bytes)
        .map_err(|error| DecodeError::Unsupported(error.to_string()))?;
    let (width, height) = picture.dimensions();
    if width.max(height) > max_edge {
        picture = picture.thumbnail(max_edge, max_edge);
    }
    let (width, height) = picture.dimensions();
    let mut pixels = picture.to_rgba8();
    // The renderer wants BGRA.
    for pixel in pixels.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Ok(Decoded {
        image: Arc::new(RenderImage::new(vec![Frame::new(pixels)])),
        width,
        height,
    })
}

/// Reads `source` and decodes it. Blocking.
pub fn decode_source(source: &Source, max_edge: u32) -> Result<Decoded, DecodeError> {
    match source {
        Source::Memory(bytes) => decode(bytes, max_edge),
        Source::Data(data) => decode(data.bytes(), max_edge),
        Source::Pasted(bytes) => decode(bytes.as_slice(), max_edge),
        Source::File(path) => {
            let bytes =
                std::fs::read(path).map_err(|error| DecodeError::Unreadable(error.to_string()))?;
            decode(&bytes, max_edge)
        }
    }
}

/// An image's place in the store.
#[derive(Clone)]
pub enum Slot {
    /// Being decoded on a background thread.
    Decoding,
    Ready(Decoded),
    Failed(DecodeError),
}

struct Entry {
    slot: Slot,
    /// Bumped each time the image is asked for, so the least recently used can be forgotten.
    used: Cell<u64>,
}

/// The decoded images, shared by every view in the window.
pub struct ImageStore {
    entries: HashMap<ImageKey, Entry>,
    tick: Cell<u64>,
    budget: u64,
    /// Images decoded since the store was made, for tests that measure the work.
    decoded: usize,
    /// How many custom emoji pictures have been decoded, so a view can tell that
    /// text it laid out with the old state needs laying out again.
    emoji_finished: u64,
}

struct GlobalStore(Entity<ImageStore>);
impl Global for GlobalStore {}

impl ImageStore {
    pub fn new(budget: u64) -> Self {
        Self {
            entries: HashMap::new(),
            tick: Cell::new(0),
            budget,
            decoded: 0,
            emoji_finished: 0,
        }
    }

    /// Makes the window's store. Called once by [`crate::init`].
    pub fn install(cx: &mut App) {
        if !cx.has_global::<GlobalStore>() {
            let store = cx.new(|_| ImageStore::new(BUDGET_BYTES));
            cx.set_global(GlobalStore(store));
        }
    }

    /// The window's store.
    pub fn global(cx: &App) -> Entity<ImageStore> {
        cx.global::<GlobalStore>().0.clone()
    }

    /// The image, if it is in the store, noting that it was used.
    pub fn get(&self, key: &ImageKey) -> Option<&Slot> {
        let entry = self.entries.get(key)?;
        let tick = self.tick.get() + 1;
        self.tick.set(tick);
        entry.used.set(tick);
        Some(&entry.slot)
    }

    /// Whether the image is known (being decoded, ready, or failed).
    pub fn has(&self, key: &ImageKey) -> bool {
        self.entries.contains_key(key)
    }

    /// How many images were decoded so far.
    pub fn decoded_count(&self) -> usize {
        self.decoded
    }

    /// How many custom emoji pictures have been decoded so far.
    pub fn emoji_generation(&self) -> u64 {
        self.emoji_finished
    }

    /// Decoded pixel bytes held now.
    pub fn held_bytes(&self) -> u64 {
        self.entries
            .values()
            .map(|entry| match &entry.slot {
                Slot::Ready(decoded) => u64::from(decoded.width) * u64::from(decoded.height) * 4,
                _ => 0,
            })
            .sum()
    }

    /// Starts decoding `source` for `key` unless the store already knows it. The work happens on
    /// a background thread; views are notified when it ends.
    pub fn request(
        &mut self,
        key: ImageKey,
        source: Source,
        max_edge: u32,
        cx: &mut Context<Self>,
    ) {
        if self.entries.contains_key(&key) {
            return;
        }
        let tick = self.tick.get() + 1;
        self.tick.set(tick);
        self.entries.insert(
            key.clone(),
            Entry {
                slot: Slot::Decoding,
                used: Cell::new(tick),
            },
        );
        let work = cx.background_spawn(async move { decode_source(&source, max_edge) });
        cx.spawn(async move |store, cx| {
            let result = work.await;
            store
                .update(cx, |store, cx| {
                    store.finish(&key, result);
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn finish(&mut self, key: &ImageKey, result: Result<Decoded, DecodeError>) {
        let Some(entry) = self.entries.get_mut(key) else {
            return;
        };
        self.decoded += 1;
        // Only a picture that is ready changes the room an emoji takes; a failed one stays `:name:`.
        if matches!(key, ImageKey::Emoji(_)) && result.is_ok() {
            self.emoji_finished += 1;
        }
        entry.slot = match result {
            Ok(decoded) => Slot::Ready(decoded),
            Err(error) => Slot::Failed(error),
        };
        self.evict(key);
    }

    /// Forgets the least recently used decoded images until the store is within its budget,
    /// never the one just finished.
    fn evict(&mut self, keep: &ImageKey) {
        while self.held_bytes() > self.budget {
            let oldest = self
                .entries
                .iter()
                .filter(|(key, entry)| *key != keep && matches!(entry.slot, Slot::Ready(_)))
                .min_by_key(|(_, entry)| entry.used.get())
                .map(|(key, _)| key.clone());
            match oldest {
                Some(key) => {
                    self.entries.remove(&key);
                }
                None => break,
            }
        }
    }

    /// Forgets an image, for one whose source changed or is gone.
    pub fn forget(&mut self, key: &ImageKey) {
        self.entries.remove(key);
    }

    /// Forgets the previews of tray chips, for when the composer's files are gone.
    pub fn forget_chips(&mut self) {
        self.entries
            .retain(|key, _| !matches!(key, ImageKey::Chip(_)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{ImageBuffer, ImageFormat, Rgba, RgbaImage};

    fn picture(width: u32, height: u32, colour: [u8; 4]) -> RgbaImage {
        ImageBuffer::from_pixel(width, height, Rgba(colour))
    }

    fn encoded(picture: &RgbaImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        // JPEG has no alpha.
        if format == ImageFormat::Jpeg {
            image::DynamicImage::ImageRgba8(picture.clone())
                .to_rgb8()
                .write_to(&mut out, format)
                .unwrap();
        } else {
            picture.write_to(&mut out, format).unwrap();
        }
        out.into_inner()
    }

    #[test]
    fn png_and_jpeg_decode_to_their_size_in_bgra() {
        let red = picture(30, 20, [255, 0, 0, 255]);
        let png = decode(&encoded(&red, ImageFormat::Png), 640).unwrap();
        assert_eq!((png.width, png.height), (30, 20));
        let jpeg = decode(&encoded(&red, ImageFormat::Jpeg), 640).unwrap();
        assert_eq!((jpeg.width, jpeg.height), (30, 20));
        // Red is the third byte of BGRA.
        let bytes = png.image.as_bytes(0).expect("one frame");
        assert_eq!(&bytes[..4], &[0, 0, 255, 255]);
    }

    #[test]
    fn a_large_image_is_scaled_to_the_cap_with_its_aspect() {
        let wide = picture(4000, 1000, [0, 128, 0, 255]);
        let decoded = decode(&encoded(&wide, ImageFormat::Png), 640).unwrap();
        assert_eq!(decoded.width, 640);
        assert_eq!(decoded.height, 160);
    }

    #[test]
    fn a_gif_shows_only_its_first_frame() {
        let mut out = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut out);
            encoder.set_repeat(Repeat::Infinite).unwrap();
            for colour in [[255u8, 0, 0, 255], [0, 0, 255, 255]] {
                encoder
                    .encode_frame(image::Frame::new(picture(8, 8, colour)))
                    .unwrap();
            }
        }
        let decoded = decode(&out, 640).unwrap();
        assert_eq!(decoded.image.frame_count(), 1, "no animation");
        let bytes = decoded.image.as_bytes(0).unwrap();
        assert_eq!(&bytes[..4], &[0, 0, 255, 255], "the first frame was red");
    }

    #[test]
    fn what_is_not_an_image_is_unsupported() {
        assert!(matches!(
            decode(b"definitely not an image", 640),
            Err(DecodeError::Unsupported(_)) | Err(DecodeError::Unreadable(_))
        ));
        assert!(decode(&[], 640).is_err());
    }

    #[test]
    fn a_missing_file_is_unreadable() {
        let error = decode_source(&Source::File("/nonexistent/lazy-test.png".into()), 640)
            .err()
            .unwrap();
        assert!(matches!(error, DecodeError::Unreadable(_)));
        assert!(!error.text().is_empty());
    }

    #[gpui::test]
    fn decoding_happens_off_the_calling_thread_and_the_store_stays_within_its_budget(
        cx: &mut gpui::TestAppContext,
    ) {
        // Room for two 100x100 images (40 KB each) and no third.
        let store = cx.new(|_| ImageStore::new(100 * 100 * 4 * 2 + 1));
        let png: Arc<[u8]> = encoded(&picture(100, 100, [1, 2, 3, 255]), ImageFormat::Png).into();
        let keys: Vec<ImageKey> = (0..3)
            .map(|n| ImageKey::Chip(format!("chip-{n}")))
            .collect();
        for key in &keys {
            store.update(cx, |store, cx| {
                store.request(key.clone(), Source::Memory(png.clone()), 640, cx)
            });
            assert!(
                matches!(
                    store.read_with(cx, |s, _| s.get(key).cloned()),
                    Some(Slot::Decoding)
                ),
                "the request returns at once, with the work still to do"
            );
            cx.run_until_parked();
        }
        let held = store.read_with(cx, |s, _| s.held_bytes());
        assert!(held <= 100 * 100 * 4 * 2 + 1, "within budget: {held}");
        assert!(
            store.read_with(cx, |s, _| s.has(&keys[2])),
            "the newest is kept"
        );
        assert!(
            !store.read_with(cx, |s, _| s.has(&keys[0])),
            "the least recently used was forgotten"
        );
        assert_eq!(store.read_with(cx, |s, _| s.decoded_count()), 3);
        // Asking for what is known does no work.
        store.update(cx, |store, cx| {
            store.request(keys[2].clone(), Source::Memory(png.clone()), 640, cx)
        });
        cx.run_until_parked();
        assert_eq!(store.read_with(cx, |s, _| s.decoded_count()), 3);
    }
}
