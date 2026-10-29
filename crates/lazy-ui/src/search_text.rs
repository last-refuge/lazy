//! Pure helpers for the search UI: the filter chips a query shows and how removing one edits the
//! text, and how a snippet's highlight ranges (counted in characters) become pieces of text that
//! are safe to style.
//!
//! The query text is the person's own, so editing it must leave everything else alone: a chip's
//! remove button takes out exactly the words that made the chip, and the spaces around them.

use std::ops::Range;

use lazy_domain::search::{FileKind, SearchQuery};
use unicode_segmentation::UnicodeSegmentation;

/// Which filter a chip stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterKind {
    From,
    In,
    After,
    Before,
    Has,
}

impl FilterKind {
    /// The word before the colon.
    pub fn key(self) -> &'static str {
        match self {
            Self::From => "from",
            Self::In => "in",
            Self::After => "after",
            Self::Before => "before",
            Self::Has => "has",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Some(match key.to_lowercase().as_str() {
            "from" => Self::From,
            "in" => Self::In,
            "after" => Self::After,
            "before" => Self::Before,
            "has" => Self::Has,
            _ => return None,
        })
    }
}

/// One filter of a parsed query, shown as a chip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chip {
    pub kind: FilterKind,
    /// The value as the query was understood: a lowercase name without `@` or `#`, a date as
    /// `2026-10-01`, or `file` or `image`.
    pub value: String,
}

impl Chip {
    /// What the chip says: `from:alice`.
    pub fn label(&self) -> String {
        format!("{}:{}", self.kind.key(), self.value)
    }

    /// What a screen reader is told about its remove button.
    pub fn remove_label(&self) -> String {
        format!("Remove filter {}", self.label())
    }
}

/// The filters a parsed query contains, in a stable order: people, channels, dates, kind of file.
pub fn chips(query: &SearchQuery) -> Vec<Chip> {
    let mut out = Vec::new();
    for name in &query.from {
        out.push(Chip {
            kind: FilterKind::From,
            value: name.clone(),
        });
    }
    for name in &query.channels {
        out.push(Chip {
            kind: FilterKind::In,
            value: name.clone(),
        });
    }
    if let Some(day) = query.after {
        out.push(Chip {
            kind: FilterKind::After,
            value: day.to_string(),
        });
    }
    if let Some(day) = query.before {
        out.push(Chip {
            kind: FilterKind::Before,
            value: day.to_string(),
        });
    }
    if let Some(kind) = query.has {
        out.push(Chip {
            kind: FilterKind::Has,
            value: match kind {
                FileKind::Any => "file",
                FileKind::Image => "image",
            }
            .to_owned(),
        });
    }
    out
}

/// A run of the query text: the byte range it covers, and for a plain word the word itself.
struct Span<'a> {
    range: Range<usize>,
    /// `None` for a quoted phrase.
    plain: Option<&'a str>,
}

/// Splits the way the query parser does: on whitespace, with a quoted run (an unclosed quote runs
/// to the end) kept together, and a quote ending a word.
fn spans(text: &str) -> Vec<Span<'_>> {
    let mut out = Vec::new();
    let mut at = 0;
    let bytes = text.as_bytes();
    while at < text.len() {
        let rest = &text[at..];
        let c = rest.chars().next().unwrap_or(' ');
        if c.is_whitespace() {
            at += c.len_utf8();
        } else if c == '"' {
            let start = at;
            at += 1;
            let end = text[at..]
                .find('"')
                .map_or(text.len(), |close| at + close + 1);
            out.push(Span {
                range: start..end,
                plain: None,
            });
            at = end;
        } else {
            let start = at;
            while at < text.len() {
                let c = text[at..].chars().next().unwrap_or(' ');
                if c.is_whitespace() || bytes[at] == b'"' {
                    break;
                }
                at += c.len_utf8();
            }
            out.push(Span {
                range: start..at,
                plain: Some(&text[start..at]),
            });
        }
    }
    out
}

/// Whether the plain word `raw` is the filter `chip` stands for. A filter that holds one value
/// (a date, the kind of file) matches by its key alone: the parser keeps only the last, so
/// removing the chip must not leave an earlier one to reappear.
fn is_filter_of(raw: &str, chip: &Chip) -> bool {
    let Some((key, value)) = raw.split_once(':') else {
        return false;
    };
    let value = value.trim();
    if value.is_empty() || FilterKind::from_key(key) != Some(chip.kind) {
        return false;
    }
    match chip.kind {
        FilterKind::From => value.trim_start_matches('@').to_lowercase() == chip.value,
        FilterKind::In => value.trim_start_matches('#').to_lowercase() == chip.value,
        FilterKind::After | FilterKind::Before | FilterKind::Has => true,
    }
}

/// The query text without the words that made `chip`, and without the spaces they leave behind.
pub fn remove_chip(text: &str, chip: &Chip) -> String {
    let mut ranges: Vec<Range<usize>> = spans(text)
        .into_iter()
        .filter(|span| span.plain.is_some_and(|raw| is_filter_of(raw, chip)))
        .map(|span| span.range)
        .collect();
    let mut out = text.to_owned();
    ranges.reverse();
    for range in ranges {
        let mut start = range.start;
        let mut end = range.end;
        // The space after the word goes with it; at the end of the text, the one before.
        let after = out[end..].len() - out[end..].trim_start().len();
        if after > 0 || end == out.len() {
            end += after;
            if end == out.len() {
                start -= out[..start].len() - out[..start].trim_end().len();
            }
        }
        out.replace_range(start..end, "");
    }
    out.trim().to_owned()
}

/// A stretch of snippet text, and whether it is a match to highlight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    /// Byte range of the snippet text.
    pub range: Range<usize>,
    pub highlighted: bool,
}

/// Cuts `text` into pieces at the ends of the highlight `ranges` (counted in characters, `end`
/// exclusive). Ranges the server got wrong cannot do harm: one past the end is clamped, an empty
/// or backwards one is dropped, overlapping ones merge, and a range that would end inside a
/// grapheme cluster (an emoji with a skin tone, a letter with a combining mark) grows to take the
/// whole cluster, so a cluster is never split between two styles. The pieces cover the whole text
/// in order and none is empty.
pub fn highlight_pieces(text: &str, ranges: &[(u32, u32)]) -> Vec<Piece> {
    if text.is_empty() {
        return Vec::new();
    }
    // Byte offset of each character, and one more for the end.
    let mut starts: Vec<usize> = text.char_indices().map(|(at, _)| at).collect();
    let chars = starts.len();
    starts.push(text.len());
    // Where clusters begin (and the end), to snap to.
    let mut edges: Vec<usize> = text.grapheme_indices(true).map(|(at, _)| at).collect();
    edges.push(text.len());
    let snap_down = |at: usize| edges[edges.partition_point(|edge| *edge <= at) - 1];
    let snap_up = |at: usize| edges[edges.partition_point(|edge| *edge < at)];

    let mut marked: Vec<Range<usize>> = ranges
        .iter()
        .filter_map(|&(start, end)| {
            let (start, end) = ((start as usize).min(chars), (end as usize).min(chars));
            (start < end).then(|| snap_down(starts[start])..snap_up(starts[end]))
        })
        .collect();
    marked.sort_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in marked {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    let mut pieces = Vec::new();
    let mut at = 0;
    for range in merged {
        if range.start > at {
            pieces.push(Piece {
                range: at..range.start,
                highlighted: false,
            });
        }
        pieces.push(Piece {
            range: range.clone(),
            highlighted: true,
        });
        at = range.end;
    }
    if at < text.len() {
        pieces.push(Piece {
            range: at..text.len(),
            highlighted: false,
        });
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chip(kind: FilterKind, value: &str) -> Chip {
        Chip {
            kind,
            value: value.to_owned(),
        }
    }

    #[test]
    fn a_parsed_query_gives_one_chip_per_filter_in_a_fixed_order() {
        let query = SearchQuery::parse(
            "budget has:image before:2026-10-01 in:#design from:@Bob from:alice after:2026-09-01",
        )
        .unwrap();
        let labels: Vec<String> = chips(&query).iter().map(Chip::label).collect();
        assert_eq!(
            labels,
            [
                "from:bob",
                "from:alice",
                "in:design",
                "after:2026-09-01",
                "before:2026-10-01",
                "has:image"
            ]
        );
        assert!(chips(&SearchQuery::parse("just words").unwrap()).is_empty());
        assert_eq!(
            chip(FilterKind::From, "bob").remove_label(),
            "Remove filter from:bob"
        );
    }

    #[test]
    fn removing_a_chip_takes_out_its_words_and_the_space_with_them() {
        let remove = |text: &str, kind, value: &str| remove_chip(text, &chip(kind, value));
        assert_eq!(
            remove("deploy from:alice failed", FilterKind::From, "alice"),
            "deploy failed"
        );
        assert_eq!(
            remove("from:alice deploy", FilterKind::From, "alice"),
            "deploy"
        );
        assert_eq!(
            remove("deploy from:alice", FilterKind::From, "alice"),
            "deploy"
        );
        assert_eq!(remove("from:alice", FilterKind::From, "alice"), "");
        // The same person written another way is the same chip; another person is left alone.
        assert_eq!(
            remove("x FROM:@Alice from:bob", FilterKind::From, "alice"),
            "x from:bob"
        );
        assert_eq!(
            remove("in:#design in:ops budget", FilterKind::In, "design"),
            "in:ops budget"
        );
    }

    #[test]
    fn a_date_or_a_kind_of_file_is_removed_wherever_it_was_written() {
        assert_eq!(
            remove_chip(
                "after:2026-01-01 plan after:2026-09-01",
                &chip(FilterKind::After, "2026-09-01")
            ),
            "plan",
            "the parser keeps the last, so removing the chip clears them all"
        );
        assert_eq!(
            remove_chip("has:file notes", &chip(FilterKind::Has, "file")),
            "notes"
        );
    }

    #[test]
    fn quoted_text_that_looks_like_a_filter_is_not_one() {
        let text = r#""from:alice said" from:alice -draft"#;
        assert_eq!(
            remove_chip(text, &chip(FilterKind::From, "alice")),
            r#""from:alice said" -draft"#
        );
        // An unclosed quote runs to the end of the text.
        assert_eq!(
            remove_chip(r#"a from:bob "from:bob"#, &chip(FilterKind::From, "bob")),
            r#"a "from:bob"#
        );
    }

    #[test]
    fn removing_what_is_not_there_changes_nothing_but_stray_edge_spaces() {
        assert_eq!(
            remove_chip("deploy  failed", &chip(FilterKind::From, "alice")),
            "deploy  failed"
        );
        assert_eq!(
            remove_chip("  deploy ", &chip(FilterKind::In, "x")),
            "deploy"
        );
    }

    #[test]
    fn text_with_multibyte_characters_is_edited_on_character_boundaries() {
        assert_eq!(
            remove_chip("café 🎉 from:zoé — résumé", &chip(FilterKind::From, "zoé")),
            "café 🎉 — résumé"
        );
    }

    fn texts(text: &str, pieces: &[Piece]) -> Vec<(String, bool)> {
        pieces
            .iter()
            .map(|p| (text[p.range.clone()].to_owned(), p.highlighted))
            .collect()
    }

    #[test]
    fn highlights_in_characters_become_byte_ranges() {
        let text = "naïve café plan";
        // "café" is characters 6..10, though it is five bytes long and "ï" takes two.
        let pieces = highlight_pieces(text, &[(6, 10)]);
        assert_eq!(
            texts(text, &pieces),
            [
                ("naïve ".to_owned(), false),
                ("café".to_owned(), true),
                (" plan".to_owned(), false)
            ]
        );
    }

    #[test]
    fn cjk_and_emoji_are_counted_as_characters() {
        let text = "日本語のテキスト 🎉 deploy 完了";
        // Characters: 日本語のテキスト is 8, then space, 🎉, space, then "deploy" starts at 11.
        let pieces = highlight_pieces(text, &[(11, 17), (0, 3)]);
        assert_eq!(
            texts(text, &pieces),
            [
                ("日本語".to_owned(), true),
                ("のテキスト 🎉 ".to_owned(), false),
                ("deploy".to_owned(), true),
                (" 完了".to_owned(), false)
            ]
        );
    }

    #[test]
    fn a_highlight_never_ends_inside_a_cluster() {
        // "é" as e + combining acute: highlighting only the e takes the accent with it.
        let text = "re\u{301}sume\u{301}";
        let pieces = highlight_pieces(text, &[(1, 2)]);
        assert_eq!(
            texts(text, &pieces),
            [
                ("r".to_owned(), false),
                ("e\u{301}".to_owned(), true),
                ("sume\u{301}".to_owned(), false)
            ]
        );
        // A family emoji (one cluster of five characters) highlighted by its first character.
        let text = "ok 👨\u{200d}👩\u{200d}👧 ok";
        let pieces = highlight_pieces(text, &[(3, 4)]);
        assert_eq!(
            texts(text, &pieces)[1],
            ("👨\u{200d}👩\u{200d}👧".to_owned(), true)
        );
        // A flag is two characters and one cluster.
        let text = "🇯🇵 trip";
        assert!(texts(text, &highlight_pieces(text, &[(1, 2)]))[0].1);
        assert_eq!(texts(text, &highlight_pieces(text, &[(1, 2)]))[0].0, "🇯🇵");
    }

    #[test]
    fn bad_ranges_are_clamped_dropped_or_merged() {
        let text = "one two three";
        // Past the end is clamped; empty and backwards are dropped.
        assert_eq!(
            texts(text, &highlight_pieces(text, &[(8, 400), (3, 3), (9, 2)])),
            [("one two ".to_owned(), false), ("three".to_owned(), true)]
        );
        assert_eq!(
            highlight_pieces(text, &[(500, 600)]),
            vec![Piece {
                range: 0..text.len(),
                highlighted: false
            }]
        );
        // Overlapping and touching ranges are one highlight, whatever order they come in.
        assert_eq!(
            texts(text, &highlight_pieces(text, &[(4, 7), (0, 5), (7, 8)])),
            [("one two ".to_owned(), true), ("three".to_owned(), false)]
        );
        assert!(highlight_pieces("", &[(0, 3)]).is_empty());
    }

    #[test]
    fn the_pieces_always_cover_the_text_exactly() {
        let text = "a\u{301}b 🎉 日本 c";
        for start in 0..14u32 {
            for end in 0..16u32 {
                let pieces = highlight_pieces(text, &[(start, end), (end, start + 2)]);
                let mut at = 0;
                for piece in &pieces {
                    assert_eq!(piece.range.start, at, "{start}..{end}");
                    assert!(piece.range.end > piece.range.start, "no empty pieces");
                    assert!(text.is_char_boundary(piece.range.start));
                    at = piece.range.end;
                }
                assert_eq!(at, text.len(), "{start}..{end}");
            }
        }
    }
}
