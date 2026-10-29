//! Text with some of its characters emphasised: the characters of a title that a fuzzy match
//! found, in the quick switcher.
//!
//! [`pieces`] cuts the text; [`styled`] draws it as one run of text, so it truncates with an
//! ellipsis as a paragraph does.

use std::ops::Range;

use gpui::{FontWeight, Hsla, SharedString, StyledText, TextRun};

use crate::emoji_font;
use crate::fonts;

/// A stretch of the text, with whether it is emphasised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    /// Byte range in the text.
    pub range: Range<usize>,
    pub emphasised: bool,
}

/// The text cut at the boundaries of the emphasised characters. `matched` holds character
/// positions (not bytes), in any order, and positions past the end are ignored. Neighbouring
/// emphasised characters are one piece; the pieces cover the text exactly.
pub fn pieces(text: &str, matched: &[usize]) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    let mut push = |range: Range<usize>, emphasised: bool| match out.last_mut() {
        Some(last) if last.emphasised == emphasised => last.range.end = range.end,
        _ => out.push(Piece { range, emphasised }),
    };
    for (position, (start, c)) in text.char_indices().enumerate() {
        push(start..start + c.len_utf8(), matched.contains(&position));
    }
    out
}

/// `text` as one run of text, the matched characters in `strong` weight and `emphasis` colour.
pub fn styled(
    text: &str,
    matched: &[usize],
    base: Hsla,
    emphasis: Hsla,
    weight: FontWeight,
) -> StyledText {
    let mut face = gpui::font(fonts::FAMILY);
    face.weight = weight;
    let mut strong = face.clone();
    strong.weight = FontWeight::BOLD;
    let runs: Vec<TextRun> = pieces(text, matched)
        .into_iter()
        .map(|piece| TextRun {
            len: piece.range.len(),
            font: if piece.emphasised {
                strong.clone()
            } else {
                face.clone()
            },
            color: if piece.emphasised { emphasis } else { base },
            background_color: None,
            underline: None,
            strikethrough: None,
        })
        .collect();
    let shared: SharedString = text.to_owned().into();
    let runs = emoji_font::with_emoji_font(&shared, runs);
    StyledText::new(shared).with_runs(runs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cut(text: &str, matched: &[usize]) -> Vec<(String, bool)> {
        pieces(text, matched)
            .into_iter()
            .map(|piece| (text[piece.range].to_owned(), piece.emphasised))
            .collect()
    }

    #[test]
    fn no_matches_is_one_plain_piece() {
        assert_eq!(cut("general", &[]), vec![("general".to_owned(), false)]);
        assert_eq!(cut("", &[0]), Vec::<(String, bool)>::new());
    }

    #[test]
    fn neighbours_join_and_gaps_separate() {
        assert_eq!(
            cut("engineering", &[0, 1, 5]),
            vec![
                ("en".to_owned(), true),
                ("gin".to_owned(), false),
                ("e".to_owned(), true),
                ("ering".to_owned(), false),
            ]
        );
    }

    #[test]
    fn positions_are_characters_not_bytes() {
        // `ë` is two bytes; position 2 is the `ë` and position 3 the `i`.
        assert_eq!(
            cut("Zoë Lindqvist", &[2, 4]),
            vec![
                ("Zo".to_owned(), false),
                ("ë".to_owned(), true),
                (" ".to_owned(), false),
                ("L".to_owned(), true),
                ("indqvist".to_owned(), false),
            ]
        );
    }

    #[test]
    fn positions_past_the_end_and_repeats_are_ignored() {
        assert_eq!(
            cut("ab", &[1, 1, 9]),
            vec![("a".to_owned(), false), ("b".to_owned(), true)]
        );
    }

    #[test]
    fn the_pieces_cover_the_text_exactly() {
        let text = "Bob Okafor, Zoë Lindqvist 🎯";
        let all = pieces(text, &[0, 3, 7, 20, 22]);
        assert_eq!(all.first().map(|p| p.range.start), Some(0));
        assert_eq!(all.last().map(|p| p.range.end), Some(text.len()));
        for pair in all.windows(2) {
            assert_eq!(pair[0].range.end, pair[1].range.start);
        }
    }
}
