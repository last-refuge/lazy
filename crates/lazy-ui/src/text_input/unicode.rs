//! Boundary and offset helpers for editing text.
//!
//! Three different units meet in a text input and must never be mixed up:
//!
//! * UTF-8 byte offsets, used internally (`&str` indexing),
//! * UTF-16 code units, the unit of the platform text-input contract,
//! * extended grapheme clusters, the unit a person perceives as one character.
//!
//! Every function here takes and returns UTF-8 byte offsets unless its name says `utf16`.

use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};

/// Which way to round an offset that falls inside a character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rounding {
    Down,
    Up,
}

/// Largest character boundary at or below `offset`, clamped to the text length.
pub fn floor_char_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// Smallest character boundary at or above `offset`, clamped to the text length.
pub fn ceil_char_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

/// Whether `offset` lies between two grapheme clusters (or at either end of the text).
pub fn is_grapheme_boundary(text: &str, offset: usize) -> bool {
    if offset == 0 || offset == text.len() {
        return true;
    }
    if offset > text.len() || !text.is_char_boundary(offset) {
        return false;
    }
    GraphemeCursor::new(offset, text.len(), true)
        .is_boundary(text, 0)
        .unwrap_or(true)
}

/// The first grapheme boundary strictly after `offset`, or the end of the text.
pub fn next_grapheme_boundary(text: &str, offset: usize) -> usize {
    let offset = floor_char_boundary(text, offset);
    if offset >= text.len() {
        return text.len();
    }
    GraphemeCursor::new(offset, text.len(), true)
        .next_boundary(text, 0)
        .ok()
        .flatten()
        .unwrap_or(text.len())
}

/// The last grapheme boundary strictly before `offset`, or zero.
pub fn prev_grapheme_boundary(text: &str, offset: usize) -> usize {
    let offset = ceil_char_boundary(text, offset);
    if offset == 0 {
        return 0;
    }
    GraphemeCursor::new(offset, text.len(), true)
        .prev_boundary(text, 0)
        .ok()
        .flatten()
        .unwrap_or(0)
}

/// Nearest grapheme boundary at or before `offset`.
pub fn snap_down(text: &str, offset: usize) -> usize {
    let offset = floor_char_boundary(text, offset);
    if is_grapheme_boundary(text, offset) {
        offset
    } else {
        prev_grapheme_boundary(text, offset)
    }
}

/// Nearest grapheme boundary at or after `offset`.
pub fn snap_up(text: &str, offset: usize) -> usize {
    let offset = ceil_char_boundary(text, offset);
    if is_grapheme_boundary(text, offset) {
        offset
    } else {
        next_grapheme_boundary(text, offset)
    }
}

/// The longest prefix of `text` that fits in `max_bytes` without splitting a grapheme cluster.
pub fn truncate_to_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    &text[..snap_down(text, max_bytes)]
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SegmentClass {
    Space,
    Word,
    Other,
}

fn classify(segment: &str) -> SegmentClass {
    if segment.chars().all(char::is_whitespace) {
        SegmentClass::Space
    } else if segment.chars().any(|c| c.is_alphanumeric()) {
        SegmentClass::Word
    } else {
        SegmentClass::Other
    }
}

/// Start of the word before `offset`: skips whitespace, then one run of letters/digits or one
/// run of punctuation and symbols (so an emoji run moves as a unit).
pub fn prev_word_boundary(text: &str, offset: usize) -> usize {
    let offset = floor_char_boundary(text, offset);
    let segments: Vec<(usize, &str)> = text[..offset].split_word_bound_indices().collect();
    let mut index = segments.len();
    while index > 0 && classify(segments[index - 1].1) == SegmentClass::Space {
        index -= 1;
    }
    if index == 0 {
        return 0;
    }
    let class = classify(segments[index - 1].1);
    while index > 0 && classify(segments[index - 1].1) == class {
        index -= 1;
    }
    segments.get(index).map_or(0, |(start, _)| *start)
}

/// End of the word after `offset`: skips whitespace, then one run of the same kind as
/// [`prev_word_boundary`].
pub fn next_word_boundary(text: &str, offset: usize) -> usize {
    let offset = floor_char_boundary(text, offset);
    let mut end = offset;
    let mut class = None;
    for (start, segment) in text[offset..].split_word_bound_indices() {
        let segment_class = classify(segment);
        match class {
            None if segment_class == SegmentClass::Space => {}
            None => class = Some(segment_class),
            Some(c) if c != segment_class => break,
            Some(_) => {}
        }
        end = offset + start + segment.len();
    }
    end
}

/// The word-segmentation unit containing `offset`, for double-click selection.
pub fn word_range_at(text: &str, offset: usize) -> std::ops::Range<usize> {
    if text.is_empty() {
        return 0..0;
    }
    let offset = floor_char_boundary(text, offset);
    let mut last = 0..0;
    for (start, segment) in text.split_word_bound_indices() {
        last = start..start + segment.len();
        if offset < last.end {
            return last;
        }
    }
    last
}

/// Start of the logical line (after the previous `\n`) containing `offset`.
pub fn line_start(text: &str, offset: usize) -> usize {
    let offset = floor_char_boundary(text, offset);
    text[..offset].rfind('\n').map_or(0, |index| index + 1)
}

/// End of the logical line (before the next `\n`) containing `offset`.
pub fn line_end(text: &str, offset: usize) -> usize {
    let offset = floor_char_boundary(text, offset);
    text[offset..]
        .find('\n')
        .map_or(text.len(), |index| offset + index)
}

/// Number of UTF-16 code units in `text`.
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Converts a byte offset to UTF-16 code units; an offset inside a character rounds down.
pub fn offset_to_utf16(text: &str, offset: usize) -> usize {
    utf16_len(&text[..floor_char_boundary(text, offset)])
}

/// Converts UTF-16 code units to a byte offset. An offset between the two halves of a surrogate
/// pair rounds according to `rounding`; past the end clamps to the text length.
pub fn offset_from_utf16(text: &str, utf16_offset: usize, rounding: Rounding) -> usize {
    let mut seen = 0;
    for (index, c) in text.char_indices() {
        if seen >= utf16_offset {
            return index;
        }
        let next = seen + c.len_utf16();
        if utf16_offset < next {
            return match rounding {
                Rounding::Down => index,
                Rounding::Up => index + c.len_utf8(),
            };
        }
        seen = next;
    }
    text.len()
}

/// Cleans text on its way into an input: CRLF and CR become LF, other control characters except
/// tab are dropped, and in single-line inputs line breaks become spaces.
pub fn normalize_input(text: &str, single_line: bool) -> String {
    let newline = if single_line { ' ' } else { '\n' };
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(newline);
            }
            '\n' => out.push(newline),
            '\t' => out.push('\t'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAMILY: &str = "👨‍👩‍👧‍👦";
    const FLAGS: &str = "🇨🇦🇯🇵";
    const COMBINED: &str = "e\u{301}";

    #[test]
    fn grapheme_boundaries_respect_clusters() {
        let text = format!("a{FAMILY}b");
        assert!(is_grapheme_boundary(&text, 0));
        assert!(is_grapheme_boundary(&text, 1));
        assert!(!is_grapheme_boundary(&text, 1 + 4));
        assert!(is_grapheme_boundary(&text, 1 + FAMILY.len()));
        assert_eq!(next_grapheme_boundary(&text, 1), 1 + FAMILY.len());
        assert_eq!(prev_grapheme_boundary(&text, 1 + FAMILY.len()), 1);
        assert_eq!(next_grapheme_boundary(&text, text.len()), text.len());
        assert_eq!(prev_grapheme_boundary(&text, 0), 0);
    }

    #[test]
    fn combining_marks_stay_with_their_base() {
        let text = format!("x{COMBINED}y");
        assert_eq!(next_grapheme_boundary(&text, 1), 1 + COMBINED.len());
        assert_eq!(snap_down(&text, 2), 1);
        assert_eq!(snap_up(&text, 2), 1 + COMBINED.len());
    }

    #[test]
    fn flags_pair_regional_indicators() {
        let first = "🇨🇦".len();
        assert_eq!(next_grapheme_boundary(FLAGS, 0), first);
        assert_eq!(next_grapheme_boundary(FLAGS, first), FLAGS.len());
        assert!(!is_grapheme_boundary(FLAGS, 4));
        assert_eq!(prev_grapheme_boundary(FLAGS, FLAGS.len()), first);
    }

    #[test]
    fn offsets_inside_a_character_are_handled() {
        let text = "a漢b";
        assert_eq!(floor_char_boundary(text, 2), 1);
        assert_eq!(ceil_char_boundary(text, 2), 4);
        assert_eq!(floor_char_boundary(text, 99), text.len());
        assert!(!is_grapheme_boundary(text, 2));
    }

    #[test]
    fn truncation_never_splits_a_cluster() {
        let text = format!("ab{FAMILY}");
        assert_eq!(truncate_to_boundary(&text, 100), text);
        assert_eq!(truncate_to_boundary(&text, 5), "ab");
        assert_eq!(truncate_to_boundary(&text, 2 + FAMILY.len()), text);
        assert_eq!(truncate_to_boundary("ab", 0), "");
    }

    #[test]
    fn word_boundaries_skip_space_and_group_punctuation() {
        let text = "hello, world... 🎉🎉 end";
        assert_eq!(next_word_boundary(text, 0), 5);
        assert_eq!(next_word_boundary(text, 5), 6);
        assert_eq!(next_word_boundary(text, 6), 12);
        assert_eq!(next_word_boundary(text, 12), 15);
        let party = text.find('🎉').unwrap();
        assert_eq!(next_word_boundary(text, 15), party + 8);
        assert_eq!(prev_word_boundary(text, text.len()), text.len() - 3);
        assert_eq!(prev_word_boundary(text, text.len() - 3), party);
        assert_eq!(prev_word_boundary(text, party), 12);
        assert_eq!(prev_word_boundary(text, 12), 7);
        assert_eq!(prev_word_boundary(text, 5), 0);
        assert_eq!(prev_word_boundary(text, 0), 0);
        assert_eq!(next_word_boundary(text, text.len()), text.len());
    }

    #[test]
    fn trailing_whitespace_moves_to_the_end() {
        assert_eq!(next_word_boundary("abc   ", 3), 6);
        assert_eq!(prev_word_boundary("   abc", 3), 0);
    }

    #[test]
    fn word_range_covers_the_segment_under_the_offset() {
        let text = "one two";
        assert_eq!(word_range_at(text, 1), 0..3);
        assert_eq!(word_range_at(text, 3), 3..4);
        assert_eq!(word_range_at(text, 5), 4..7);
        assert_eq!(word_range_at(text, 7), 4..7);
        assert_eq!(word_range_at("", 0), 0..0);
    }

    #[test]
    fn line_boundaries_follow_newlines() {
        let text = "ab\ncd\n\nef";
        assert_eq!(line_start(text, 4), 3);
        assert_eq!(line_end(text, 4), 5);
        assert_eq!(line_start(text, 6), 6);
        assert_eq!(line_end(text, 6), 6);
        assert_eq!(line_end(text, 8), 9);
        assert_eq!(line_start(text, 0), 0);
    }

    #[test]
    fn utf16_conversion_round_trips_at_every_char_boundary() {
        let text = format!("a漢{FAMILY}é{COMBINED}🎉z");
        for (offset, _) in text.char_indices().chain([(text.len(), ' ')]) {
            let units = offset_to_utf16(&text, offset);
            assert_eq!(offset_from_utf16(&text, units, Rounding::Down), offset);
            assert_eq!(offset_from_utf16(&text, units, Rounding::Up), offset);
        }
        assert_eq!(utf16_len(&text), text.encode_utf16().count());
    }

    #[test]
    fn utf16_offsets_inside_surrogate_pairs_round_by_request() {
        let text = "a🎉b";
        assert_eq!(offset_from_utf16(text, 2, Rounding::Down), 1);
        assert_eq!(offset_from_utf16(text, 2, Rounding::Up), 5);
        assert_eq!(offset_from_utf16(text, 99, Rounding::Down), text.len());
        assert_eq!(offset_to_utf16(text, 3), 1);
    }

    #[test]
    fn normalization_converts_line_endings_and_strips_controls() {
        assert_eq!(normalize_input("a\r\nb\rc\nd", false), "a\nb\nc\nd");
        assert_eq!(normalize_input("a\r\nb\rc\nd", true), "a b c d");
        assert_eq!(normalize_input("x\u{7}\u{1b}y\tz", false), "xy\tz");
        assert_eq!(normalize_input("🎉 é", false), "🎉 é");
    }
}
