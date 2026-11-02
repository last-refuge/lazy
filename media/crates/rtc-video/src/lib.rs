//! Video for the rtc media library: frames, where they come from, and how they are compressed.
//!
//! - [`Frame`]: an image in BGRA (blue, green, red, alpha; 8 bits each), the order GPUI and most
//!   platform surfaces draw.
//! - [`FrameStore`]: the newest frame of every tile, shared between the threads that decode or
//!   capture and the thread that draws. A drawer polls it; nothing is queued.
//! - [`source`]: a camera (feature `camera`) or a moving test pattern.
//! - [`codec`] (feature `codec`): H.264 through OpenH264, built from source.

#[cfg(feature = "codec")]
pub mod codec;
pub mod source;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// An image in BGRA, rows top to bottom, no padding.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Frame({}x{})", self.width, self.height)
    }
}

impl Frame {
    /// A frame of one color.
    pub fn solid(width: u32, height: u32, [b, g, r]: [u8; 3]) -> Frame {
        let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
        for _ in 0..width * height {
            bgra.extend_from_slice(&[b, g, r, 255]);
        }
        Frame {
            width,
            height,
            bgra,
        }
    }

    /// From packed RGB.
    pub fn from_rgb(width: u32, height: u32, rgb: &[u8]) -> Frame {
        let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
        for pixel in rgb
            .as_chunks::<3>()
            .0
            .iter()
            .take(width as usize * height as usize)
        {
            bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
        Frame {
            width,
            height,
            bgra,
        }
    }

    /// The pixel at (`x`, `y`) as (b, g, r).
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        let at = (y as usize * self.width as usize + x as usize) * 4;
        [self.bgra[at], self.bgra[at + 1], self.bgra[at + 2]]
    }

    /// Scaled down (never up) to fit within `max_width` by `max_height`, keeping its shape, with
    /// even sides (what the encoder needs). Nearest-neighbor: cheap, and enough for a call tile.
    pub fn fit_within(self, max_width: u32, max_height: u32) -> Frame {
        let scale = f64::min(
            1.0,
            f64::min(
                f64::from(max_width) / f64::from(self.width.max(1)),
                f64::from(max_height) / f64::from(self.height.max(1)),
            ),
        );
        let width = ((f64::from(self.width) * scale) as u32).max(2) & !1;
        let height = ((f64::from(self.height) * scale) as u32).max(2) & !1;
        if width == self.width && height == self.height {
            return self;
        }
        let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            let sy = (u64::from(y) * u64::from(self.height) / u64::from(height)) as usize;
            let row = sy * self.width as usize;
            for x in 0..width {
                let sx = (u64::from(x) * u64::from(self.width) / u64::from(width)) as usize;
                let at = (row + sx) * 4;
                bgra.extend_from_slice(&self.bgra[at..at + 4]);
            }
        }
        Frame {
            width,
            height,
            bgra,
        }
    }

    /// Mirrored left to right: how people expect to see themselves.
    pub fn mirrored(&self) -> Frame {
        let mut bgra = Vec::with_capacity(self.bgra.len());
        let row_len = self.width as usize * 4;
        for row in self.bgra.chunks_exact(row_len) {
            for pixel in row.as_chunks::<4>().0.iter().rev() {
                bgra.extend_from_slice(pixel);
            }
        }
        Frame {
            width: self.width,
            height: self.height,
            bgra,
        }
    }
}

/// Whose picture a tile shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TileKey {
    /// This client's own camera (a preview, mirrored).
    Local,
    /// A participant, by the SFU's number for them.
    Remote(u64),
}

/// A tile's newest frame, and a number that changes with every new one.
#[derive(Clone, Debug)]
pub struct Tile {
    pub frame: Arc<Frame>,
    pub serial: u64,
}

/// The newest frame of every tile. Cheap to clone; every clone is the same store.
#[derive(Clone, Default)]
pub struct FrameStore {
    inner: Arc<Mutex<Store>>,
}

#[derive(Default)]
struct Store {
    tiles: HashMap<TileKey, Tile>,
    serial: u64,
}

impl std::fmt::Debug for FrameStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FrameStore({} tiles)", self.keys().len())
    }
}

impl FrameStore {
    pub fn new() -> FrameStore {
        FrameStore::default()
    }

    /// Replaces `key`'s frame.
    pub fn put(&self, key: TileKey, frame: Frame) {
        let mut store = self.inner.lock().expect("frame store lock");
        store.serial += 1;
        let serial = store.serial;
        store.tiles.insert(
            key,
            Tile {
                frame: Arc::new(frame),
                serial,
            },
        );
    }

    /// The tile's newest frame, if it has one.
    pub fn get(&self, key: TileKey) -> Option<Tile> {
        self.inner
            .lock()
            .expect("frame store lock")
            .tiles
            .get(&key)
            .cloned()
    }

    /// Forgets `key`'s frame (the camera went off, or the person left).
    pub fn remove(&self, key: TileKey) {
        self.inner
            .lock()
            .expect("frame store lock")
            .tiles
            .remove(&key);
    }

    /// Forgets everything (the call ended).
    pub fn clear(&self) {
        self.inner.lock().expect("frame store lock").tiles.clear();
    }

    /// A number that changes whenever any frame does: a drawer redraws when it moves.
    pub fn serial(&self) -> u64 {
        self.inner.lock().expect("frame store lock").serial
    }

    /// The tiles that have a frame.
    pub fn keys(&self) -> Vec<TileKey> {
        self.inner
            .lock()
            .expect("frame store lock")
            .tiles
            .keys()
            .copied()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_scale_down_to_even_sides_and_never_up() {
        let frame = Frame::solid(1280, 720, [1, 2, 3]);
        let small = frame.clone().fit_within(640, 640);
        assert_eq!((small.width, small.height), (640, 360));
        assert_eq!(small.pixel(10, 10), [1, 2, 3]);
        let odd = Frame::solid(101, 51, [0, 0, 0]).fit_within(1000, 1000);
        assert_eq!((odd.width, odd.height), (100, 50));
        assert_eq!(odd.bgra.len(), 100 * 50 * 4);
    }

    #[test]
    fn mirroring_swaps_left_and_right() {
        let mut frame = Frame::solid(2, 1, [0, 0, 0]);
        frame.bgra[0] = 9;
        assert_eq!(frame.mirrored().pixel(1, 0), [9, 0, 0]);
    }

    #[test]
    fn the_store_keeps_the_newest_frame_per_tile() {
        let store = FrameStore::new();
        assert!(store.get(TileKey::Local).is_none());
        store.put(TileKey::Remote(1), Frame::solid(2, 2, [1, 1, 1]));
        let first = store.serial();
        store.put(TileKey::Remote(1), Frame::solid(2, 2, [2, 2, 2]));
        assert!(store.serial() > first);
        assert_eq!(
            store.get(TileKey::Remote(1)).unwrap().frame.pixel(0, 0),
            [2, 2, 2]
        );
        store.remove(TileKey::Remote(1));
        assert!(store.keys().is_empty());
    }
}
