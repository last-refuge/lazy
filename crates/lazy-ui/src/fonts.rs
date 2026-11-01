//! The bundled typefaces.
//!
//! Lazy ships DM Sans (SIL Open Font License 1.1, see `assets/fonts/dm-sans`) for interface text
//! and Nunito Bold (also OFL 1.1, `assets/fonts/nunito`) for headings, so text looks and weighs the
//! same on every machine rather than depending on the system's default font, which may lack a
//! semibold weight. Both are static instances of the brand's variable fonts, because the text
//! engine cannot pick a weight from a variable font (`assets/fonts/make-static-fonts.py`). Lilex
//! (OFL 1.1, `assets/fonts/lilex`) is the monospace face for code. Noto Color Emoji (OFL 1.1,
//! `assets/fonts/noto-color-emoji`, with one added `cmap` entry) draws emoji in color; see
//! [`crate::emoji_font`] for why it is needed and how it is used. Scripts the faces do not cover
//! fall back to system fonts.
//!
//! DM Sans comes in regular and semibold, each with a true italic (no synthetic italics). Nunito is
//! only ever used at weight 700, for headings: dialog titles, the headings of empty states, and the
//! sign-in screens (see [`heading`]).

use std::borrow::Cow;

use gpui::{App, FontWeight, Styled};

/// Family name to request in text styles: the interface's typeface.
pub const FAMILY: &str = "DM Sans";
/// Family name of the heading face (Nunito Bold). Request it with [`FontWeight::BOLD`], or use
/// [`heading`].
pub const HEADING_FAMILY: &str = "Nunito";
/// Family name of the monospace face used for code.
pub const MONO_FAMILY: &str = "Lilex";

/// Every bundled font file, for hosts and tests that register them on their own text system.
pub const FILES: [&[u8]; 8] = [
    include_bytes!("../assets/fonts/dm-sans/DMSans-Regular.ttf"),
    include_bytes!("../assets/fonts/dm-sans/DMSans-Italic.ttf"),
    include_bytes!("../assets/fonts/dm-sans/DMSans-SemiBold.ttf"),
    include_bytes!("../assets/fonts/dm-sans/DMSans-SemiBoldItalic.ttf"),
    include_bytes!("../assets/fonts/nunito/Nunito-Bold.ttf"),
    include_bytes!("../assets/fonts/lilex/Lilex-Regular.ttf"),
    include_bytes!("../assets/fonts/lilex/Lilex-Bold.ttf"),
    include_bytes!("../assets/fonts/noto-color-emoji/NotoColorEmoji.ttf"),
];

/// Draws an element's text as a heading: Nunito at bold weight.
pub fn heading<E: Styled>(element: E) -> E {
    element
        .font_family(HEADING_FAMILY)
        .font_weight(FontWeight::BOLD)
}

/// Whether the bundled emoji font (the last of [`FILES`]) is registered. CoreText cannot load it,
/// so macOS draws emoji with Apple Color Emoji through system fallback.
pub const BUNDLED_EMOJI: bool = !cfg!(target_os = "macos");

/// Registers the bundled fonts with the text system. A failure is logged and the app falls back
/// to system fonts.
pub fn register(cx: &App) {
    let count = if BUNDLED_EMOJI {
        FILES.len()
    } else {
        FILES.len() - 1
    };
    let fonts = FILES[..count]
        .iter()
        .map(|bytes| Cow::Borrowed(*bytes))
        .collect();
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        tracing::warn!(%error, "could not load the bundled fonts; using system fonts");
    }
}
