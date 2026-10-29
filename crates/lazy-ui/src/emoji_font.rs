//! Emoji drawn in color, as the emoji they are.
//!
//! GPUI draws a character with the first font that has it. On Linux the fallback list puts
//! text fonts with symbol glyphs (DejaVu Sans, Noto Sans Symbols) ahead of the color emoji font,
//! so `❤️`, `☕`, `☀️`, `✈️`, and even `😁` came out as small black outlines, and `1️⃣` as a
//! bare box. The fallback order cannot be changed from outside the text system, and the font
//! fallback list on a text style cannot name an emoji font either: GPUI refuses to load a family
//! that has no `m` (it measures with it) and an emoji font has no letters.
//!
//! So the emoji are not left to fall back. Lazy bundles Noto Color Emoji with one added `cmap`
//! entry for `m` (see `assets/fonts/noto-color-emoji/README.md`), which makes it loadable by name,
//! and every text run that shows a message, a reaction, a picker, or the composer is split at emoji
//! so that each emoji, as a whole grapheme cluster (a skin tone, a flag, a family joined with
//! zero-width joiners, a keycap), is shaped by that font alone.
//!
//! What counts as an emoji follows Unicode's presentation rule. A cluster is an emoji when it
//! is spelled exactly as the emoji dataset spells it (`☕`, `❤️`, `👍🏽`, `🏳️‍🌈`) or carries the
//! emoji variation selector U+FE0F. A bare `❤` or `☀` is text by default and stays text, as it
//! does in other apps; the picker and the shortcode converter always write the dataset spelling.
//!
//! Only text that goes through this module gets the fix. Emoji in other places, such as a channel
//! topic or a person's display name, still use whatever font the system falls back to.

use gpui::{
    Font, FontWeight, Hsla, SharedString, StrikethroughStyle, StyledText, TextRun, UnderlineStyle,
    font,
};
use lazy_domain::emoji::{self, Emoji, SkinTone};
use unicode_segmentation::UnicodeSegmentation;

use crate::fonts::FAMILY as TEXT_FAMILY;

/// Family name of the bundled color emoji font.
pub const FAMILY: &str = "Noto Color Emoji";

/// The newest Emoji version whose skin-toned glyphs the bundled font has. The dataset is newer than
/// the font: an emoji from a later version can have a base glyph in the font but no toned one, and
/// a toned sequence the font cannot join draws as the base emoji with a bare color swatch beside it.
/// The picker shows such emoji untoned.
pub const FONT_EMOJI_VERSION: (u32, u32) = (17, 0);

/// `emoji` in `tone`, or untoned if the font cannot draw that tone of it (see
/// [`FONT_EMOJI_VERSION`]).
pub fn drawable_tone(emoji: &'static Emoji, tone: SkinTone) -> &'static Emoji {
    let toned = lazy_domain::emoji::with_tone(emoji, tone);
    let version = toned.emoji_version();
    if (version.major(), version.minor()) > FONT_EMOJI_VERSION {
        emoji
    } else {
        toned
    }
}

/// The variation selector that asks for emoji presentation.
const EMOJI_PRESENTATION: char = '\u{fe0f}';

/// The face emoji are drawn with.
pub fn face() -> Font {
    font(FAMILY)
}

/// Whether one grapheme cluster is an emoji that should be drawn in color.
pub fn is_emoji_cluster(cluster: &str) -> bool {
    if cluster.is_ascii() {
        return false;
    }
    cluster.contains(EMOJI_PRESENTATION)
        || emoji::lookup(cluster).is_some_and(|known| known.as_str() == cluster)
}

/// Splits `runs` so that every emoji cluster in `text` is drawn in the emoji font. Everything else
/// about a run (color, underline, strikethrough, background) carries over, and lengths still add
/// up to `text.len()`. Text with no non-ASCII character is returned untouched without looking at it.
///
/// `runs` must cover `text` exactly, as GPUI requires.
pub fn with_emoji_font(text: &str, runs: Vec<TextRun>) -> Vec<TextRun> {
    if text.is_ascii() {
        return runs;
    }
    let mut out: Vec<TextRun> = Vec::with_capacity(runs.len() + 2);
    let mut at = 0;
    for run in runs {
        let end = at + run.len;
        let slice = &text[at..end];
        at = end;
        if slice.is_ascii() {
            out.push(run);
            continue;
        }
        let mut piece_start = 0;
        let mut piece_is_emoji = None;
        let flush = |from: usize, to: usize, emoji: bool, out: &mut Vec<TextRun>| {
            if to > from {
                let mut piece = run.clone();
                piece.len = to - from;
                if emoji {
                    piece.font = face();
                }
                out.push(piece);
            }
        };
        for (offset, cluster) in slice.grapheme_indices(true) {
            let is_emoji = is_emoji_cluster(cluster);
            match piece_is_emoji {
                Some(current) if current != is_emoji => {
                    flush(piece_start, offset, current, &mut out);
                    piece_start = offset;
                    piece_is_emoji = Some(is_emoji);
                }
                None => piece_is_emoji = Some(is_emoji),
                Some(_) => {}
            }
        }
        if let Some(current) = piece_is_emoji {
            flush(piece_start, slice.len(), current, &mut out);
        }
    }
    out
}

/// One line of text in the UI's text face, in `color` and `weight`, with its emoji in color. For
/// labels that are not part of a message (menu items, tooltips, the picker's preview), where the
/// parent element supplies the size.
pub fn label(text: impl Into<SharedString>, color: Hsla, weight: FontWeight) -> StyledText {
    let text: SharedString = text.into();
    let mut face = font(TEXT_FAMILY);
    face.weight = weight;
    let base = TextRun {
        len: text.len(),
        font: face,
        color,
        background_color: None,
        underline: None::<UnderlineStyle>,
        strikethrough: None::<StrikethroughStyle>,
    };
    let runs = with_emoji_font(&text, vec![base]);
    StyledText::new(text).with_runs(runs)
}

/// A single emoji, or several, drawn in color and nothing else.
pub fn glyphs(text: impl Into<SharedString>) -> StyledText {
    let text: SharedString = text.into();
    let run = TextRun {
        len: text.len(),
        font: face(),
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    StyledText::new(text).with_runs(vec![run])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(len: usize) -> TextRun {
        TextRun {
            len,
            font: font(TEXT_FAMILY),
            color: gpui::black(),
            background_color: None,
            underline: Some(UnderlineStyle {
                thickness: gpui::px(1.),
                color: None,
                wavy: false,
            }),
            strikethrough: None,
        }
    }

    fn is_emoji_font(run: &TextRun) -> bool {
        run.font.family.as_ref() == FAMILY
    }

    #[test]
    fn what_counts_as_an_emoji_follows_the_presentation_rule() {
        for emoji in [
            "🎉",
            "😁",
            "☕",
            "❤️",
            "✈️",
            "☀️",
            "👍🏽",
            "🇨🇦",
            "🏳️‍🌈",
            "👨‍👩‍👧‍👦",
            "1️⃣",
            "#️⃣",
            "👩🏽‍🚀",
        ] {
            assert!(is_emoji_cluster(emoji), "{emoji} is an emoji");
        }
        for text in [
            "a", "1", "#", "é", "日", "—", "™", "©", "→", "❤", "☀", "✌", "\u{200d}",
        ] {
            assert!(!is_emoji_cluster(text), "{text:?} is text");
        }
    }

    #[test]
    fn ascii_text_is_returned_untouched() {
        let runs = vec![run(5)];
        assert_eq!(with_emoji_font("hello", runs.clone()).len(), 1);
        assert_eq!(with_emoji_font("hello", runs)[0].font, font(TEXT_FAMILY));
    }

    #[test]
    fn each_emoji_gets_its_own_font_and_the_rest_keeps_its_own() {
        let text = "ship 🚢 now ❤️❤️ ok 日本語";
        let runs = with_emoji_font(text, vec![run(text.len())]);
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), text.len());
        let pieces: Vec<(&str, bool)> = {
            let mut at = 0;
            runs.iter()
                .map(|r| {
                    let piece = &text[at..at + r.len];
                    at += r.len;
                    (piece, is_emoji_font(r))
                })
                .collect()
        };
        assert_eq!(
            pieces,
            vec![
                ("ship ", false),
                ("🚢", true),
                (" now ", false),
                ("❤️❤️", true),
                (" ok 日本語", false),
            ]
        );
        assert!(
            runs.iter().all(|r| r.underline.is_some()),
            "decoration carries over"
        );
    }

    #[test]
    fn a_whole_cluster_is_one_piece() {
        let text = "👨‍👩‍👧‍👦🏳️‍🌈1️⃣🇨🇦";
        let runs = with_emoji_font(text, vec![run(text.len())]);
        assert_eq!(runs.len(), 1, "adjacent emoji share a run");
        assert!(is_emoji_font(&runs[0]));
    }

    #[test]
    fn runs_with_different_styles_are_split_independently() {
        let text = "a🎉b🎉";
        let runs = with_emoji_font(text, vec![run(1), run(4), run(1), run(4)]);
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), text.len());
        assert_eq!(runs.iter().filter(|r| is_emoji_font(r)).count(), 2);
    }

    #[test]
    fn a_tone_the_font_cannot_draw_falls_back_to_the_untoned_emoji() {
        let thumbs_up = emoji::lookup("👍").unwrap();
        assert_eq!(
            drawable_tone(thumbs_up, SkinTone::Dark).as_str(),
            "👍🏿",
            "an old emoji keeps its tone"
        );
        // Unicode 18's thumb signs are newer than the bundled font.
        let newer = emoji::lookup("\u{1faf9}").unwrap();
        assert_eq!(drawable_tone(newer, SkinTone::Dark), newer);
    }

    #[test]
    fn bare_text_presentation_symbols_stay_text() {
        let text = "I ❤ it ☀";
        let runs = with_emoji_font(text, vec![run(text.len())]);
        assert_eq!(runs.len(), 1);
        assert!(!is_emoji_font(&runs[0]));
    }
}
