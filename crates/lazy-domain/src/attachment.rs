//! Rules for attached files: sizes, names, and which media types are what they say they are and
//! which may be shown inline.
//!
//! These are pure rules shared by the server (which enforces them on upload and download) and the
//! clients (which use them to choose an icon or a preview and to refuse the obvious early).

/// Largest file, in bytes.
pub const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
/// Most files on one message.
pub const MAX_ATTACHMENTS_PER_MESSAGE: usize = 10;
/// Longest file name, in bytes.
pub const MAX_ATTACHMENT_NAME_BYTES: usize = 255;
/// The long side of a thumbnail, in pixels.
pub const THUMBNAIL_MAX_EDGE: u32 = 640;
/// Most pixels an image may have and still be decoded (a guard against decompression bombs).
pub const MAX_IMAGE_PIXELS: u64 = 64_000_000;
/// Hours an upload that was never attached to a message is kept.
pub const PENDING_ATTACHMENT_TTL_HOURS: i64 = 24;
/// The media type of a file whose type is unknown or unsafe to state.
pub const OCTET_STREAM: &str = "application/octet-stream";

/// A file name that is safe to store and to offer for download: path separators, control
/// characters, and a leading dot or space are removed, and the result is cut to
/// [`MAX_ATTACHMENT_NAME_BYTES`] at a character boundary. A name with nothing left is `file`.
pub fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            // Directionality overrides can disguise an extension (`photo\u{202e}gpj.exe`).
            '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_start_matches('.').trim();
    let mut out = String::new();
    for c in trimmed.chars() {
        if out.len() + c.len_utf8() > MAX_ATTACHMENT_NAME_BYTES {
            break;
        }
        out.push(c);
    }
    let out = out.trim_end().to_owned();
    if out.is_empty() {
        "file".to_owned()
    } else {
        out
    }
}

/// The media type the first bytes of a file say it is, for the types worth recognising: images
/// that can be shown inline, and a few common containers.
pub fn sniff_media_type(prefix: &[u8]) -> Option<&'static str> {
    const SIGNATURES: [(&[u8], &str); 7] = [
        (b"\x89PNG\r\n\x1a\n", "image/png"),
        (b"\xff\xd8\xff", "image/jpeg"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"%PDF-", "application/pdf"),
        (b"PK\x03\x04", "application/zip"),
        (b"\x1f\x8b", "application/gzip"),
    ];
    for (signature, media_type) in SIGNATURES {
        if prefix.starts_with(signature) {
            return Some(media_type);
        }
    }
    if prefix.len() >= 12 && &prefix[..4] == b"RIFF" && &prefix[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Whether `media_type` is shaped like one (`type/subtype`, lowercase ASCII tokens).
pub fn media_type_is_well_formed(media_type: &str) -> bool {
    let Some((kind, subtype)) = media_type.split_once('/') else {
        return false;
    };
    let token = |part: &str| {
        !part.is_empty()
            && part.len() <= 64
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "+-.".contains(c))
    };
    token(kind) && token(subtype)
}

/// Types a browser or viewer would run or render as active content. A file claiming one is
/// stored and served as [`OCTET_STREAM`].
fn is_active_content(media_type: &str) -> bool {
    matches!(
        media_type,
        "text/html"
            | "application/xhtml+xml"
            | "image/svg+xml"
            | "text/xml"
            | "application/xml"
            | "text/javascript"
            | "application/javascript"
            | "application/x-javascript"
            | "text/css"
            | "application/wasm"
    ) || media_type.ends_with("+xml")
}

/// Whether files of this type may be shown inline (as an image preview): the raster formats the
/// thumbnail generator decodes. Everything else is offered as a download only.
pub fn is_previewable(media_type: &str) -> bool {
    matches!(
        media_type,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

/// The media type to store and serve for a file, from what the uploader declared and what its
/// first bytes say.
///
/// The bytes win when they are recognised, so a file cannot pass as an image by claiming to be
/// one. An unrecognised file keeps its declared type only if that is well formed and not active
/// content (HTML, SVG, scripts, stylesheets); otherwise it is [`OCTET_STREAM`].
pub fn resolve_media_type(declared: &str, prefix: &[u8]) -> String {
    if let Some(sniffed) = sniff_media_type(prefix) {
        return sniffed.to_owned();
    }
    let declared = declared
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if media_type_is_well_formed(&declared)
        && !is_active_content(&declared)
        && !is_previewable(&declared)
    {
        declared
    } else {
        OCTET_STREAM.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_lose_paths_controls_and_disguises() {
        assert_eq!(sanitize_file_name("report.pdf"), "report.pdf");
        assert_eq!(sanitize_file_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(
            sanitize_file_name("C:\\Users\\a\\b.txt"),
            "C__Users_a_b.txt"
        );
        assert_eq!(sanitize_file_name(".hidden"), "hidden");
        assert_eq!(sanitize_file_name("  spaced name.txt  "), "spaced name.txt");
        assert_eq!(sanitize_file_name("a\u{0}b\nc.txt"), "a_b_c.txt");
        assert_eq!(sanitize_file_name("photo\u{202e}gpj.exe"), "photo_gpj.exe");
        assert_eq!(sanitize_file_name("..."), "file");
        assert_eq!(sanitize_file_name(""), "file");
        assert_eq!(sanitize_file_name("日本語 ☕.txt"), "日本語 ☕.txt");
    }

    #[test]
    fn long_names_are_cut_on_a_character_boundary() {
        let long = "é".repeat(300);
        let cut = sanitize_file_name(&long);
        assert!(cut.len() <= MAX_ATTACHMENT_NAME_BYTES);
        assert!(cut.chars().all(|c| c == 'é'));
        let emoji = "☕".repeat(200);
        assert!(sanitize_file_name(&emoji).len() <= MAX_ATTACHMENT_NAME_BYTES);
    }

    #[test]
    fn signatures_identify_files() {
        assert_eq!(
            sniff_media_type(b"\x89PNG\r\n\x1a\nrest"),
            Some("image/png")
        );
        assert_eq!(sniff_media_type(b"\xff\xd8\xff\xe0"), Some("image/jpeg"));
        assert_eq!(sniff_media_type(b"GIF89a.."), Some("image/gif"));
        assert_eq!(
            sniff_media_type(b"RIFF\x10\0\0\0WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(sniff_media_type(b"RIFF\x10\0\0\0WAVEfmt "), None);
        assert_eq!(sniff_media_type(b"%PDF-1.7"), Some("application/pdf"));
        assert_eq!(sniff_media_type(b"PK\x03\x04"), Some("application/zip"));
        assert_eq!(sniff_media_type(b"hello"), None);
        assert_eq!(sniff_media_type(b""), None);
    }

    #[test]
    fn what_the_bytes_say_beats_what_the_uploader_claims() {
        // A zip claiming to be an image is a zip; an image claiming to be a zip is an image.
        assert_eq!(
            resolve_media_type("image/png", b"PK\x03\x04"),
            "application/zip"
        );
        assert_eq!(
            resolve_media_type("application/zip", b"\x89PNG\r\n\x1a\n"),
            "image/png"
        );
        // Unrecognised bytes cannot pass as an image or as active content.
        assert_eq!(resolve_media_type("image/png", b"not a png"), OCTET_STREAM);
        assert_eq!(resolve_media_type("text/html", b"<script>"), OCTET_STREAM);
        assert_eq!(resolve_media_type("image/svg+xml", b"<svg/>"), OCTET_STREAM);
        assert_eq!(
            resolve_media_type("application/atom+xml", b"<a/>"),
            OCTET_STREAM
        );
        assert_eq!(resolve_media_type("text/css", b"a{}"), OCTET_STREAM);
        // Plain, well-formed types survive, with parameters dropped and case folded.
        assert_eq!(
            resolve_media_type("Text/Plain; charset=utf-8", b"hi"),
            "text/plain"
        );
        assert_eq!(
            resolve_media_type("application/json", b"{}"),
            "application/json"
        );
        assert_eq!(resolve_media_type("text/csv", b"a,b"), "text/csv");
        // Nonsense is a download.
        for bad in [
            "",
            "plain",
            "a/b/c",
            "text/",
            "/x",
            "T E/x",
            &"a".repeat(80),
        ] {
            assert_eq!(resolve_media_type(bad, b"x"), OCTET_STREAM, "{bad:?}");
        }
    }

    #[test]
    fn only_raster_images_are_previewable() {
        for yes in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
            assert!(is_previewable(yes));
        }
        for no in [
            "image/svg+xml",
            "application/pdf",
            "text/html",
            "video/mp4",
            "",
        ] {
            assert!(!is_previewable(no));
        }
    }
}
