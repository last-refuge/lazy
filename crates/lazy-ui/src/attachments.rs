//! How attached files are presented: sizes, names, icons, and where an image goes in its box.
//!
//! These are pure rules (no GPUI), shared by the composer's tray, the message rows, and the
//! viewer, so the wording and geometry can be tested as plain values.
//!
//! # The copy format
//!
//! A selection that includes a message's attachments writes one line per file where the file sits
//! in the message, after the message's text: `[file: name (size)]`, with the size as
//! [`format_size`] writes it, for example `[file: report.pdf (1.2 MB)]`. See [`copy_marker`].

use std::path::PathBuf;

use gpui::{ClipboardEntry, ClipboardItem, ImageFormat};

/// What a paste brought that is not text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pasted {
    /// Copied files, by path.
    Files(Vec<PathBuf>),
    /// A copied picture (a screenshot, an image copied from a page), as the clipboard holds it.
    Image { bytes: Vec<u8>, format: ImageFormat },
}

/// Whether the clipboard holds files or a picture to attach, and which.
///
/// Files win. A picture counts only when there is no text with it: copying a table or a styled
/// paragraph puts a picture of it beside its text, and that is a paste of the text. SVG is not an
/// attachable picture (it is never shown inline), so it is left to the text path.
pub fn classify_clipboard(item: &ClipboardItem) -> Option<Pasted> {
    let files: Vec<PathBuf> = item
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            ClipboardEntry::ExternalPaths(paths) => Some(paths.paths().to_vec()),
            _ => None,
        })
        .flatten()
        .collect();
    if !files.is_empty() {
        return Some(Pasted::Files(files));
    }
    if item.text().is_some_and(|text| !text.trim().is_empty()) {
        return None;
    }
    item.entries().iter().find_map(|entry| match entry {
        ClipboardEntry::Image(image)
            if image.format != ImageFormat::Svg && !image.bytes.is_empty() =>
        {
            Some(Pasted::Image {
                bytes: image.bytes.clone(),
                format: image.format,
            })
        }
        _ => None,
    })
}

/// A picture from the clipboard as PNG bytes, which is what attaching one takes. A PNG is passed
/// through; any other format is decoded and written as PNG. Blocking: run it off the main thread.
pub fn to_png(bytes: Vec<u8>, format: ImageFormat) -> Result<Vec<u8>, String> {
    if format == ImageFormat::Png {
        return Ok(bytes);
    }
    let picture = image::load_from_memory(&bytes).map_err(|error| error.to_string())?;
    let mut out = std::io::Cursor::new(Vec::new());
    picture
        .write_to(&mut out, image::ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    Ok(out.into_inner())
}

/// The widest an image preview may be, in logical pixels.
pub const PREVIEW_MAX_WIDTH: f32 = 360.0;
/// The tallest an image preview may be, in logical pixels.
pub const PREVIEW_MAX_HEIGHT: f32 = 240.0;
/// The box used for a preview whose pixel size is not known (it never is, for a decoded image).
pub const UNKNOWN_PREVIEW: (f32, f32) = (240.0, 160.0);
/// Chips the composer's tray shows; the rest are summarised in one line.
pub const MAX_CHIPS_SHOWN: usize = 10;

/// A file size the way people read it: `340 bytes`, `1.2 KB`, `25 MB`.
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    if bytes < 1024 {
        return if bytes == 1 {
            "1 byte".to_owned()
        } else {
            format!("{bytes} bytes")
        };
    }
    let mut value = bytes as f64 / KB;
    let mut unit = "KB";
    for next in ["MB", "GB", "TB"] {
        if value < 1024.0 {
            break;
        }
        value /= KB;
        unit = next;
    }
    // One decimal below 10, none above, so `1.2 MB` and `25 MB` and never `25.0 MB`.
    if value < 10.0 {
        format!("{value:.1} {unit}")
    } else {
        format!("{value:.0} {unit}")
    }
}

/// `name` shortened to at most `max` characters by cutting the middle, keeping the start and the
/// end (the extension is what tells files apart). Shorter names are returned as they are.
pub fn middle_ellipsis(name: &str, max: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= max || max < 5 {
        return name.to_owned();
    }
    let keep = max - 1;
    // The tail gets the extension and a little more; the head gets the rest.
    let tail = (keep / 2).max(
        name.rfind('.')
            .map(|dot| name[dot..].chars().count() + 2)
            .unwrap_or(0)
            .min(keep.saturating_sub(2)),
    );
    let head = keep - tail;
    let head: String = chars[..head].iter().collect();
    let tail: String = chars[chars.len() - tail..].iter().collect();
    format!("{head}…{tail}")
}

/// The size an image of `width` by `height` pixels takes in a box of at most `max_width` by
/// `max_height`: scaled down to fit with its aspect kept, never scaled up.
pub fn fit_within(width: u32, height: u32, max_width: f32, max_height: f32) -> (f32, f32) {
    if width == 0 || height == 0 {
        return UNKNOWN_PREVIEW;
    }
    let (w, h) = (width as f32, height as f32);
    let scale = (max_width / w).min(max_height / h).min(1.0);
    ((w * scale).max(1.0), (h * scale).max(1.0))
}

/// The preview box of an attachment: its pixel size fitted into [`PREVIEW_MAX_WIDTH`] by
/// [`PREVIEW_MAX_HEIGHT`], or [`UNKNOWN_PREVIEW`] when the size is not known.
pub fn preview_box(width: Option<u32>, height: Option<u32>) -> (f32, f32) {
    preview_box_within(width, height, PREVIEW_MAX_WIDTH)
}

/// [`preview_box`] for a column only `available` pixels wide (the thread pane): never wider than
/// the column, never above the usual box, aspect kept.
pub fn preview_box_within(width: Option<u32>, height: Option<u32>, available: f32) -> (f32, f32) {
    let max_width = PREVIEW_MAX_WIDTH.min(available.max(48.0));
    match (width, height) {
        (Some(w), Some(h)) => fit_within(w, h, max_width, PREVIEW_MAX_HEIGHT),
        _ => {
            let (w, h) = UNKNOWN_PREVIEW;
            let scale = (max_width / w).min(1.0);
            (w * scale, h * scale)
        }
    }
}

/// What kind of file a chip stands for, which picks its icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Image,
    Pdf,
    Archive,
    Text,
    Audio,
    Video,
    Other,
}

impl FileKind {
    /// The short text drawn in the type icon.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Image => "IMG",
            Self::Pdf => "PDF",
            Self::Archive => "ZIP",
            Self::Text => "TXT",
            Self::Audio => "AUD",
            Self::Video => "VID",
            Self::Other => "FILE",
        }
    }

    /// What a screen reader should call the icon.
    pub fn label(self) -> &'static str {
        match self {
            Self::Image => "Image",
            Self::Pdf => "PDF document",
            Self::Archive => "Archive",
            Self::Text => "Text file",
            Self::Audio => "Audio file",
            Self::Video => "Video file",
            Self::Other => "File",
        }
    }
}

/// The kind of a file from its media type, and for an unspecific one (`application/octet-stream`)
/// from its extension.
pub fn file_kind(media_type: &str, name: &str) -> FileKind {
    let media_type = media_type.to_ascii_lowercase();
    match media_type.as_str() {
        "application/pdf" => return FileKind::Pdf,
        "application/zip"
        | "application/gzip"
        | "application/x-tar"
        | "application/x-7z-compressed"
        | "application/x-bzip2"
        | "application/x-xz"
        | "application/vnd.rar" => {
            return FileKind::Archive;
        }
        _ => {}
    }
    if media_type.starts_with("image/") {
        return FileKind::Image;
    }
    if media_type.starts_with("audio/") {
        return FileKind::Audio;
    }
    if media_type.starts_with("video/") {
        return FileKind::Video;
    }
    if media_type.starts_with("text/") {
        return FileKind::Text;
    }
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "pdf" => FileKind::Pdf,
        "zip" | "gz" | "tgz" | "tar" | "7z" | "bz2" | "xz" | "rar" => FileKind::Archive,
        "txt" | "md" | "csv" | "log" | "json" | "toml" | "yaml" | "yml" => FileKind::Text,
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "opus" => FileKind::Audio,
        "mp4" | "mov" | "mkv" | "webm" | "avi" => FileKind::Video,
        "png" | "jpg" | "jpeg" | "gif" | "webp" => FileKind::Image,
        _ => FileKind::Other,
    }
}

/// The line a copy writes for an attached file: `[file: name (size)]`.
pub fn copy_marker(name: &str, size: u64) -> String {
    format!("[file: {name} ({})]", format_size(size))
}

/// What the tray says when more files were added than it can show.
pub fn more_files_text(hidden: usize) -> String {
    if hidden == 1 {
        "and 1 more file".to_owned()
    } else {
        format!("and {hidden} more files")
    }
}

/// The note under the tray when a message already has the most files it can carry.
pub fn too_many_files_text(limit: usize) -> String {
    format!("A message can have at most {limit} files. Remove one to add another.")
}

/// A file that is over the size limit, refused before anything is uploaded.
pub fn too_large_text(name: &str, size: u64, limit: u64) -> String {
    format!(
        "{name} is {}, which is more than the {} limit.",
        format_size(size),
        format_size(limit)
    )
}

/// Progress as a fraction in `0.0..=1.0`; an unknown total is no progress yet.
pub fn fraction(sent: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (sent as f64 / total as f64).clamp(0.0, 1.0) as f32
    }
}

/// The line in place of the composer's hint while a file in the tray did not upload, because the
/// message would be sent without it.
pub fn failed_file_blocks_send_text() -> &'static str {
    "Retry or remove the file that didn't upload, then send."
}

/// What the person is told when the file picker could not be opened.
pub fn picker_failed_text(reason: &str) -> String {
    format!("Couldn't open the file picker: {reason}")
}

/// What is said of a dropped or chosen folder, which cannot be attached.
pub fn folder_text(name: &str) -> String {
    format!("{name} is a folder. Only files can be attached.")
}

/// Where a saved file went, in a few words: the name of its folder, or the whole path when
/// there is no folder name to give.
pub fn saved_where(path: &std::path::Path) -> String {
    path.parent()
        .and_then(|folder| folder.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// `Uploading 2 of 3 files`, for a message waiting on its files.
pub fn uploading_text(done: usize, total: usize) -> String {
    if total == 1 {
        "Uploading 1 file".to_owned()
    } else {
        format!("Uploading {done} of {total} files")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_like_a_person_says_them() {
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(1), "1 byte");
        assert_eq!(format_size(340), "340 bytes");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(10 * 1024), "10 KB");
        assert_eq!(format_size(1_258_291), "1.2 MB");
        assert_eq!(format_size(25 * 1024 * 1024), "25 MB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn a_long_name_loses_its_middle_and_keeps_its_extension() {
        assert_eq!(middle_ellipsis("report.pdf", 20), "report.pdf");
        let cut = middle_ellipsis("quarterly-financial-summary-final-v3.pdf", 24);
        assert!(cut.chars().count() <= 24, "{cut}");
        assert!(cut.starts_with("quarterly"), "{cut}");
        assert!(cut.ends_with(".pdf"), "{cut}");
        assert!(cut.contains('…'), "{cut}");
        let no_extension = middle_ellipsis(&"a".repeat(60), 20);
        assert_eq!(no_extension.chars().count(), 20);
        // Characters, not bytes.
        let wide = middle_ellipsis(&"日本語".repeat(20), 10);
        assert_eq!(wide.chars().count(), 10);
    }

    #[test]
    fn previews_fit_the_box_keep_their_aspect_and_never_grow() {
        assert_eq!(fit_within(720, 480, 360.0, 240.0), (360.0, 240.0));
        let (w, h) = fit_within(4000, 1000, 360.0, 240.0);
        assert_eq!(w, 360.0);
        assert!((h - 90.0).abs() < 0.01, "{h}");
        let (w, h) = fit_within(1000, 4000, 360.0, 240.0);
        assert_eq!(h, 240.0);
        assert!((w - 60.0).abs() < 0.01, "{w}");
        assert_eq!(fit_within(100, 50, 360.0, 240.0), (100.0, 50.0));
        assert_eq!(fit_within(0, 50, 360.0, 240.0), UNKNOWN_PREVIEW);
        assert_eq!(preview_box(None, Some(10)), UNKNOWN_PREVIEW);
        // A narrow column shrinks the box, aspect kept.
        let (w, h) = preview_box_within(Some(1000), Some(500), 200.0);
        assert_eq!((w, h), (200.0, 100.0));
        assert_eq!(
            preview_box_within(Some(1000), Some(500), 1000.0),
            (360.0, 180.0)
        );
    }

    #[test]
    fn files_are_told_apart_by_type_then_by_extension() {
        assert_eq!(file_kind("application/pdf", "x"), FileKind::Pdf);
        assert_eq!(file_kind("application/zip", "x"), FileKind::Archive);
        assert_eq!(file_kind("audio/mpeg", "x"), FileKind::Audio);
        assert_eq!(file_kind("video/mp4", "x"), FileKind::Video);
        assert_eq!(file_kind("text/plain", "x"), FileKind::Text);
        assert_eq!(file_kind("image/png", "x"), FileKind::Image);
        assert_eq!(
            file_kind("application/octet-stream", "notes.TXT"),
            FileKind::Text
        );
        assert_eq!(
            file_kind("application/octet-stream", "song.flac"),
            FileKind::Audio
        );
        assert_eq!(
            file_kind("application/octet-stream", "blob"),
            FileKind::Other
        );
    }

    #[test]
    fn the_copy_marker_names_the_file_and_its_size() {
        assert_eq!(
            copy_marker("report.pdf", 1_258_291),
            "[file: report.pdf (1.2 MB)]"
        );
    }

    #[test]
    fn the_notes_say_what_to_do() {
        assert_eq!(more_files_text(1), "and 1 more file");
        assert_eq!(more_files_text(4), "and 4 more files");
        assert!(too_many_files_text(10).contains("at most 10 files"));
        let text = too_large_text("movie.mkv", 40 * 1024 * 1024, 25 * 1024 * 1024);
        assert_eq!(
            text,
            "movie.mkv is 40 MB, which is more than the 25 MB limit."
        );
        assert_eq!(uploading_text(2, 3), "Uploading 2 of 3 files");
        assert_eq!(uploading_text(0, 1), "Uploading 1 file");
    }

    #[test]
    fn files_on_the_clipboard_win_and_a_picture_beside_text_is_a_text_paste() {
        let files = ClipboardItem {
            entries: vec![
                ClipboardEntry::String(gpui::ClipboardString::new("/tmp/a.png".into())),
                ClipboardEntry::ExternalPaths(gpui::ExternalPaths(
                    vec![PathBuf::from("/tmp/a.png")].into(),
                )),
            ],
        };
        assert_eq!(
            classify_clipboard(&files),
            Some(Pasted::Files(vec![PathBuf::from("/tmp/a.png")]))
        );
        let png = gpui::Image::from_bytes(ImageFormat::Png, vec![1, 2, 3]);
        let alone = ClipboardItem::new_image(&png);
        assert!(matches!(
            classify_clipboard(&alone),
            Some(Pasted::Image { .. })
        ));
        let beside_text = ClipboardItem {
            entries: vec![
                ClipboardEntry::String(gpui::ClipboardString::new("a cell".into())),
                ClipboardEntry::Image(png),
            ],
        };
        assert_eq!(classify_clipboard(&beside_text), None);
        assert_eq!(
            classify_clipboard(&ClipboardItem::new_string("hello".into())),
            None
        );
        let svg = gpui::Image::from_bytes(ImageFormat::Svg, b"<svg/>".to_vec());
        assert_eq!(classify_clipboard(&ClipboardItem::new_image(&svg)), None);
    }

    #[test]
    fn a_pasted_picture_becomes_png() {
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 4, image::Rgb([9, 9, 9])))
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();
        let png = to_png(jpeg.into_inner(), ImageFormat::Jpeg).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        assert_eq!(to_png(vec![1, 2], ImageFormat::Png).unwrap(), vec![1, 2]);
        assert!(to_png(b"junk".to_vec(), ImageFormat::Webp).is_err());
    }

    #[test]
    fn a_saved_file_is_placed_by_its_folder() {
        assert_eq!(
            saved_where(std::path::Path::new("/home/alice/Downloads/report.pdf")),
            "Downloads"
        );
        assert_eq!(
            saved_where(std::path::Path::new("report.pdf")),
            "report.pdf"
        );
    }

    #[test]
    fn progress_is_a_clamped_fraction() {
        assert_eq!(fraction(0, 0), 0.0);
        assert_eq!(fraction(50, 100), 0.5);
        assert_eq!(fraction(500, 100), 1.0);
    }
}
