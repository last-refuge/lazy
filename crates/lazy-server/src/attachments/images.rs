//! Reading the dimensions of an uploaded image and making its thumbnail.
//!
//! Everything here is blocking and allocates in proportion to the image, so the caller runs it on
//! the blocking thread pool behind a semaphore (see [`crate::attachments`]). The header is read
//! first and the pixel count checked against [`MAX_IMAGE_PIXELS`] before any pixel is decoded, so
//! a decompression bomb is refused for the price of its header.

use std::io::{self, BufReader, Cursor};

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use lazy_domain::attachment::{MAX_IMAGE_PIXELS, THUMBNAIL_MAX_EDGE};

use super::store::BlockingSource;

/// JPEG quality of a thumbnail.
const JPEG_QUALITY: u8 = 80;

/// What was learned about an image.
pub struct Analysis {
    /// The size the image is displayed at: its pixels with the camera's rotation applied.
    pub width: u32,
    pub height: u32,
    pub thumbnail: Vec<u8>,
    /// `image/jpeg`, or `image/png` for an image with transparency.
    pub thumbnail_type: &'static str,
}

/// Why an image was refused. The message is for the person who uploaded it.
#[derive(Debug)]
pub struct Refused(pub String);

fn format_of(media_type: &str) -> Option<ImageFormat> {
    match media_type {
        "image/png" => Some(ImageFormat::Png),
        "image/jpeg" => Some(ImageFormat::Jpeg),
        "image/gif" => Some(ImageFormat::Gif),
        "image/webp" => Some(ImageFormat::WebP),
        _ => None,
    }
}

/// Opens an image, checks its pixel count against `max_pixels` from the header before decoding a
/// single pixel, decodes it (a GIF or WebP as its first frame) and applies the rotation its
/// metadata asks for, so the result is the right way up.
pub fn decode_upright(
    media_type: &str,
    max_pixels: u64,
    open: impl Fn() -> io::Result<Box<dyn BlockingSource>>,
) -> Result<DynamicImage, Refused> {
    let format = format_of(media_type)
        .ok_or_else(|| Refused("that kind of image cannot be shown".into()))?;
    let source = open().map_err(|_| Refused("the image could not be read".into()))?;
    let undecodable = |_: image::ImageError| Refused("the image cannot be decoded".into());
    let mut decoder = ImageReader::with_format(BufReader::new(source), format)
        .into_decoder()
        .map_err(undecodable)?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 {
        return Err(Refused("the image has no pixels".into()));
    }
    if u64::from(width) * u64::from(height) > max_pixels {
        return Err(Refused(format!(
            "the image has {width} by {height} pixels; the most is {max_pixels} in all"
        )));
    }
    let orientation = decoder.orientation().map_err(undecodable)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(undecodable)?;
    image.apply_orientation(orientation);
    Ok(image)
}

/// Reads an image's size, refuses it if it has too many pixels, decodes it, and makes its
/// thumbnail: the long side at most [`THUMBNAIL_MAX_EDGE`], as JPEG at quality 80, or as PNG if
/// the image has any transparency. A GIF or WebP is its first frame, and a photo is the right way
/// up. `open` opens the image afresh each time it is called.
pub fn analyze(
    media_type: &str,
    open: impl Fn() -> io::Result<Box<dyn BlockingSource>>,
) -> Result<Analysis, Refused> {
    let upright = decode_upright(media_type, MAX_IMAGE_PIXELS, open)?;
    let (width, height) = (upright.width(), upright.height());
    let (thumbnail, thumbnail_type) =
        thumbnail_of(upright).map_err(|_| Refused("the image cannot be re-encoded".into()))?;
    Ok(Analysis {
        width,
        height,
        thumbnail,
        thumbnail_type,
    })
}

/// A custom emoji's image, ready to store.
pub struct EmojiImage {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Turns an uploaded image into a custom emoji's picture: upright, scaled down so its long side is
/// at most `edge` pixels with the aspect kept (never enlarged), as an RGBA PNG.
pub fn emoji_image(
    media_type: &str,
    max_pixels: u64,
    edge: u32,
    open: impl Fn() -> io::Result<Box<dyn BlockingSource>>,
) -> Result<EmojiImage, Refused> {
    let upright = decode_upright(media_type, max_pixels, open)?;
    let scaled = if upright.width().max(upright.height()) > edge {
        upright.resize(edge, edge, FilterType::Lanczos3)
    } else {
        upright
    };
    let rgba = DynamicImage::ImageRgba8(scaled.to_rgba8());
    let mut out = Cursor::new(Vec::new());
    rgba.write_with_encoder(PngEncoder::new(&mut out))
        .map_err(|_| Refused("the image cannot be re-encoded".into()))?;
    Ok(EmojiImage {
        png: out.into_inner(),
        width: rgba.width(),
        height: rgba.height(),
    })
}

/// Turns an uploaded image into an avatar: upright, the largest centred square cut from it, scaled
/// to exactly `size` pixels on a side (a smaller picture is enlarged), as an RGBA PNG.
pub fn avatar_image(
    media_type: &str,
    max_pixels: u64,
    size: u32,
    open: impl Fn() -> io::Result<Box<dyn BlockingSource>>,
) -> Result<Vec<u8>, Refused> {
    let upright = decode_upright(media_type, max_pixels, open)?;
    let (width, height) = (upright.width(), upright.height());
    let side = width.min(height);
    let square = upright.crop_imm((width - side) / 2, (height - side) / 2, side, side);
    let scaled = DynamicImage::ImageRgba8(
        square
            .resize_exact(size, size, FilterType::Lanczos3)
            .to_rgba8(),
    );
    let mut out = Cursor::new(Vec::new());
    scaled
        .write_with_encoder(PngEncoder::new(&mut out))
        .map_err(|_| Refused("the image cannot be re-encoded".into()))?;
    Ok(out.into_inner())
}

fn has_transparency(image: &DynamicImage) -> bool {
    image.color().has_alpha() && image.to_rgba8().pixels().any(|pixel| pixel.0[3] != u8::MAX)
}

fn thumbnail_of(image: DynamicImage) -> Result<(Vec<u8>, &'static str), image::ImageError> {
    let (width, height) = (image.width(), image.height());
    let scaled = if width.max(height) > THUMBNAIL_MAX_EDGE {
        image.resize(THUMBNAIL_MAX_EDGE, THUMBNAIL_MAX_EDGE, FilterType::Triangle)
    } else {
        image
    };
    let mut out = Cursor::new(Vec::new());
    if has_transparency(&scaled) {
        scaled.write_with_encoder(PngEncoder::new(&mut out))?;
        Ok((out.into_inner(), "image/png"))
    } else {
        // JPEG has no alpha channel and no 16-bit samples.
        DynamicImage::ImageRgb8(scaled.to_rgb8())
            .write_with_encoder(JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY))?;
        Ok((out.into_inner(), "image/jpeg"))
    }
}

#[cfg(test)]
mod tests {
    use image::{ImageEncoder, Rgba, RgbaImage};

    use super::*;

    fn png(width: u32, height: u32, alpha: u8) -> Vec<u8> {
        let image = RgbaImage::from_pixel(width, height, Rgba([200, 30, 30, alpha]));
        let mut out = Vec::new();
        PngEncoder::new(&mut out)
            .write_image(
                image.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        out
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    fn open(bytes: Vec<u8>) -> impl Fn() -> io::Result<Box<dyn BlockingSource>> {
        move || Ok(Box::new(Cursor::new(bytes.clone())))
    }

    fn decoded(thumbnail: &[u8]) -> DynamicImage {
        image::load_from_memory(thumbnail).unwrap()
    }

    #[test]
    fn a_large_opaque_image_gets_a_jpeg_thumbnail_that_fits() {
        let result = analyze("image/png", open(png(1600, 800, 255))).unwrap();
        assert_eq!((result.width, result.height), (1600, 800));
        assert_eq!(result.thumbnail_type, "image/jpeg");
        let thumb = decoded(&result.thumbnail);
        assert_eq!((thumb.width(), thumb.height()), (640, 320));
    }

    #[test]
    fn an_image_with_transparency_gets_a_png_and_a_small_one_keeps_its_size() {
        let result = analyze("image/png", open(png(100, 50, 100))).unwrap();
        assert_eq!(result.thumbnail_type, "image/png");
        let thumb = decoded(&result.thumbnail);
        assert_eq!((thumb.width(), thumb.height()), (100, 50));
        assert_eq!(thumb.to_rgba8().get_pixel(0, 0).0[3], 100);
    }

    #[test]
    fn an_image_that_is_not_what_it_says_is_refused() {
        let refused = analyze("image/png", open(b"\x89PNG\r\n\x1a\nnot really".to_vec()));
        assert!(refused.is_err());
        assert!(analyze("image/jpeg", open(png(10, 10, 255))).is_err());
        assert!(analyze("application/pdf", open(vec![1, 2, 3])).is_err());
    }

    #[test]
    fn an_avatar_is_the_centre_square_scaled_to_exactly_the_size_asked() {
        // 300 by 200: red left of x = 50, green after it, blue from x = 250. The centre square is
        // x from 50 to 250, so only green is left.
        let image = RgbaImage::from_fn(300, 200, |x, _| {
            if x < 50 {
                Rgba([230, 20, 20, 255])
            } else if x < 250 {
                Rgba([20, 230, 20, 255])
            } else {
                Rgba([20, 20, 230, 255])
            }
        });
        let mut wide = Vec::new();
        PngEncoder::new(&mut wide)
            .write_image(image.as_raw(), 300, 200, image::ExtendedColorType::Rgba8)
            .unwrap();
        let out = avatar_image("image/png", 1_000_000, 64, open(wide)).unwrap();
        let avatar = decoded(&out).to_rgba8();
        assert_eq!(avatar.dimensions(), (64, 64));
        for (x, y) in [(1, 1), (62, 1), (1, 62), (62, 62), (32, 32)] {
            let [r, g, b, a] = avatar.get_pixel(x, y).0;
            assert!(
                g > 200 && r < 60 && b < 60 && a == 255,
                "{x},{y}: {r},{g},{b},{a}"
            );
        }
        // A tall one is cut the same way, and a small one is enlarged.
        let tall = avatar_image("image/png", 1_000_000, 64, open(png(20, 90, 255))).unwrap();
        assert_eq!(decoded(&tall).to_rgba8().dimensions(), (64, 64));
        let tiny = avatar_image("image/png", 1_000_000, 64, open(png(3, 3, 255))).unwrap();
        assert_eq!(decoded(&tiny).to_rgba8().dimensions(), (64, 64));
    }

    #[test]
    fn an_avatar_keeps_transparency_and_refuses_more_pixels_than_allowed() {
        let out = avatar_image("image/png", 1_000_000, 32, open(png(40, 40, 100))).unwrap();
        assert_eq!(decoded(&out).to_rgba8().get_pixel(16, 16).0[3], 100);
        let Err(Refused(why)) = avatar_image("image/png", 1_000, 32, open(png(40, 40, 255))) else {
            panic!("an image over the pixel bound was accepted");
        };
        assert!(why.contains("pixels"), "{why}");
        assert!(avatar_image("image/png", 1_000_000, 32, open(b"nope".to_vec())).is_err());
    }

    #[test]
    fn too_many_pixels_are_refused_from_the_header_alone() {
        // A PNG header that claims 10,000 by 10,000 pixels (100 million), with no pixel data. The
        // refusal must come from the size, before any decoding is tried.
        let mut header = png(1, 1, 255);
        header[16..20].copy_from_slice(&10_000u32.to_be_bytes());
        header[20..24].copy_from_slice(&10_000u32.to_be_bytes());
        // The header chunk's checksum covers its type and data (bytes 12 to 29).
        let checksum = crc32(&header[12..29]);
        header[29..33].copy_from_slice(&checksum.to_be_bytes());
        let Err(Refused(why)) = analyze("image/png", open(header)) else {
            panic!("a pixel bomb was accepted");
        };
        assert!(why.contains("pixels"), "{why}");
    }
}
