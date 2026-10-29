//! The bundled typeface.
//!
//! Lazy ships IBM Plex Sans (SIL Open Font License 1.1, see `assets/fonts/ibm-plex-sans`) so text
//! looks and weighs the same on every machine rather than depending on the system's default font,
//! which may lack a semibold weight, and Lilex (also OFL 1.1, `assets/fonts/lilex`) as the
//! monospace face for code. Noto Color Emoji (OFL 1.1, `assets/fonts/noto-color-emoji`, with one
//! added `cmap` entry) draws emoji in color; see [`crate::emoji_font`] for why it is needed and
//! how it is used. Scripts the faces do not cover fall back to system fonts.

use std::borrow::Cow;

use gpui::App;

/// Family name to request in text styles.
pub const FAMILY: &str = "IBM Plex Sans";
/// Family name of the monospace face used for code.
pub const MONO_FAMILY: &str = "Lilex";

/// Every bundled font file, for hosts and tests that register them on their own text system.
pub const FILES: [&[u8]; 7] = [
    include_bytes!("../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf"),
    include_bytes!("../assets/fonts/ibm-plex-sans/IBMPlexSans-Italic.ttf"),
    include_bytes!("../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf"),
    include_bytes!("../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBoldItalic.ttf"),
    include_bytes!("../assets/fonts/lilex/Lilex-Regular.ttf"),
    include_bytes!("../assets/fonts/lilex/Lilex-Bold.ttf"),
    include_bytes!("../assets/fonts/noto-color-emoji/NotoColorEmoji.ttf"),
];

/// Registers the bundled fonts with the text system. A failure is logged and the app falls back
/// to system fonts.
pub fn register(cx: &App) {
    let fonts = FILES.iter().map(|bytes| Cow::Borrowed(*bytes)).collect();
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        tracing::warn!(%error, "could not load the bundled fonts; using system fonts");
    }
}
