//! The bundled typeface.
//!
//! Lazy ships IBM Plex Sans (SIL Open Font License 1.1, see `assets/fonts/ibm-plex-sans`) so text
//! looks and weighs the same on every machine rather than depending on the system's default font,
//! which may lack a semibold weight. Emoji and scripts the face does not cover fall back to
//! system fonts.

use std::borrow::Cow;

use gpui::App;

/// Family name to request in text styles.
pub const FAMILY: &str = "IBM Plex Sans";

const FILES: [&[u8]; 4] = [
    include_bytes!("../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf"),
    include_bytes!("../assets/fonts/ibm-plex-sans/IBMPlexSans-Italic.ttf"),
    include_bytes!("../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBold.ttf"),
    include_bytes!("../assets/fonts/ibm-plex-sans/IBMPlexSans-SemiBoldItalic.ttf"),
];

/// Registers the bundled fonts with the text system. A failure is logged and the app falls back
/// to system fonts.
pub fn register(cx: &App) {
    let fonts = FILES.iter().map(|bytes| Cow::Borrowed(*bytes)).collect();
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        tracing::warn!(%error, "could not load the bundled fonts; using system fonts");
    }
}
