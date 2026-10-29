//! Selecting text across the messages of a timeline.
//!
//! A selection is a range over the conversation's *loaded messages*, not over what is on screen.
//! Each end is a [`Pos`]: which message (by identity, so it survives rows being added above or
//! below) and a byte offset into that message's selectable text (see
//! [`crate::rich_text::message_text`]). Nothing here knows about rows, scrolling, or drawing:
//! this module resolves a selection against the messages as they are now, says which part of each
//! message is selected, and builds the text a copy puts on the clipboard. That is what lets a
//! selection reach messages that are scrolled out of view, and keeps it in place while older
//! history is prepended or new messages arrive.
//!
//! # What copying writes
//!
//! A selection inside one message copies just the selected text. A selection across messages
//! copies each message as `Author  time` on one line, then its selected text, with a blank line
//! between messages.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

/// One end of a selection: a position in one message's selectable text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos<K> {
    /// The message, by identity.
    pub key: K,
    /// A byte offset into the message's selectable text, on a grapheme boundary.
    pub offset: usize,
}

/// A selection: where it began (`anchor`) and the end that moves when it is extended (`head`).
/// The head may come before the anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection<K> {
    pub anchor: Pos<K>,
    pub head: Pos<K>,
}

impl<K: Copy + Eq> Selection<K> {
    /// A selection that has not covered anything yet: both ends in one place.
    pub fn caret(at: Pos<K>) -> Self {
        Self {
            anchor: at,
            head: at,
        }
    }

    /// Nothing is covered.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// A selection laid against the messages now loaded, in document order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    pub first_row: usize,
    pub first_offset: usize,
    pub last_row: usize,
    pub last_offset: usize,
}

impl Resolved {
    pub fn is_empty(&self) -> bool {
        self.first_row == self.last_row && self.first_offset == self.last_offset
    }
}

/// Finds the rows of both ends with `index_of` and puts them in document order. `None` when an
/// end's message is not loaded any more (it was deleted): there is then nothing to show or copy.
pub fn resolve<K: Copy + Eq>(
    selection: &Selection<K>,
    index_of: impl Fn(K) -> Option<usize>,
) -> Option<Resolved> {
    let anchor = (index_of(selection.anchor.key)?, selection.anchor.offset);
    let head = (index_of(selection.head.key)?, selection.head.offset);
    let (first, last) = if anchor <= head {
        (anchor, head)
    } else {
        (head, anchor)
    };
    Some(Resolved {
        first_row: first.0,
        first_offset: first.1,
        last_row: last.0,
        last_offset: last.1,
    })
}

/// The part of row `row`'s text, of `len` bytes, that is selected, if any.
pub fn row_selection(resolved: &Resolved, row: usize, len: usize) -> Option<Range<usize>> {
    if row < resolved.first_row || row > resolved.last_row {
        return None;
    }
    let start = if row == resolved.first_row {
        resolved.first_offset.min(len)
    } else {
        0
    };
    let end = if row == resolved.last_row {
        resolved.last_offset.min(len)
    } else {
        len
    };
    (start < end).then_some(start..end)
}

/// The nearest grapheme-cluster boundary of `text` to `offset` (a boundary of its own stays; a
/// position inside a cluster goes to whichever edge is closer, the start on a tie), so a
/// selection never splits an emoji sequence or a letter and its combining marks.
pub fn snap(text: &str, offset: usize) -> usize {
    if offset >= text.len() {
        return text.len();
    }
    let mut before = 0;
    for (start, cluster) in text.grapheme_indices(true) {
        let end = start + cluster.len();
        if offset == start {
            return start;
        }
        if offset < end {
            return if offset - start <= end - offset {
                start
            } else {
                end
            };
        }
        before = end;
    }
    before
}

/// The word around `offset`, as the Unicode word-break rules see it: a double-click's selection.
/// On a space it is the run of spaces; at the very end of the text it is the last word. Chinese,
/// Japanese, and Korean text has no spaces to break on, and the rules treat each ideograph as its
/// own word, so a double-click there selects one character.
pub fn word_at(text: &str, offset: usize) -> Range<usize> {
    let mut last = 0..0;
    for (start, word) in text.split_word_bound_indices() {
        let end = start + word.len();
        if offset < end {
            return start..end;
        }
        last = start..end;
    }
    last
}

/// What a message contributes to a copy that spans several messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyRow {
    pub author: String,
    pub time: String,
    /// The message's selectable text.
    pub text: String,
}

/// The text a copy writes. `row` supplies a loaded message by its index.
pub fn copy_text(resolved: &Resolved, row: impl Fn(usize) -> Option<CopyRow>) -> String {
    if resolved.is_empty() {
        return String::new();
    }
    if resolved.first_row == resolved.last_row {
        let Some(row) = row(resolved.first_row) else {
            return String::new();
        };
        let start = snap(&row.text, resolved.first_offset);
        let end = snap(&row.text, resolved.last_offset);
        return row.text[start..end].to_owned();
    }
    let mut blocks = Vec::new();
    for index in resolved.first_row..=resolved.last_row {
        let Some(row) = row(index) else {
            continue;
        };
        let range = row_selection(resolved, index, row.text.len());
        let part = range
            .map(|range| {
                row.text[snap(&row.text, range.start)..snap(&row.text, range.end)].to_owned()
            })
            .unwrap_or_default();
        // The end rows may be selected only at their very edge: nothing of them is copied.
        if part.is_empty() && (index == resolved.first_row || index == resolved.last_row) {
            continue;
        }
        blocks.push(format!("{}  {}\n{}", row.author, row.time, part));
    }
    blocks.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    type Sel = Selection<u32>;

    fn pos(key: u32, offset: usize) -> Pos<u32> {
        Pos { key, offset }
    }

    fn index(key: u32) -> Option<usize> {
        // Messages 10, 20, 30, 40 sit at rows 0 to 3; 99 is not loaded.
        [10, 20, 30, 40].iter().position(|k| *k == key)
    }

    #[test]
    fn a_selection_is_put_in_document_order_whichever_way_it_was_dragged() {
        let down = Sel {
            anchor: pos(10, 4),
            head: pos(30, 2),
        };
        let up = Sel {
            anchor: pos(30, 2),
            head: pos(10, 4),
        };
        let expected = Resolved {
            first_row: 0,
            first_offset: 4,
            last_row: 2,
            last_offset: 2,
        };
        assert_eq!(resolve(&down, index), Some(expected));
        assert_eq!(resolve(&up, index), Some(expected));
        // Within one message the smaller offset comes first.
        let within = Sel {
            anchor: pos(20, 9),
            head: pos(20, 3),
        };
        let resolved = resolve(&within, index).unwrap();
        assert_eq!((resolved.first_offset, resolved.last_offset), (3, 9));
    }

    #[test]
    fn a_selection_whose_message_is_gone_resolves_to_nothing() {
        let gone = Sel {
            anchor: pos(10, 0),
            head: pos(99, 3),
        };
        assert_eq!(resolve(&gone, index), None);
    }

    #[test]
    fn the_same_selection_follows_its_messages_when_rows_are_added_above() {
        // The anchor and head name messages, not rows: prepending older history moves their rows
        // and leaves the selection on the same text.
        let selection = Sel {
            anchor: pos(20, 1),
            head: pos(30, 5),
        };
        let before = resolve(&selection, index).unwrap();
        let shifted = |key: u32| index(key).map(|row| row + 50);
        let after = resolve(&selection, shifted).unwrap();
        assert_eq!((before.first_row, after.first_row), (1, 51));
        assert_eq!((before.last_row, after.last_row), (2, 52));
        assert_eq!(before.first_offset, after.first_offset);
        assert_eq!(before.last_offset, after.last_offset);
    }

    #[test]
    fn each_row_knows_which_part_of_it_is_selected() {
        let resolved = Resolved {
            first_row: 1,
            first_offset: 4,
            last_row: 3,
            last_offset: 2,
        };
        assert_eq!(row_selection(&resolved, 0, 10), None, "above the selection");
        assert_eq!(
            row_selection(&resolved, 1, 10),
            Some(4..10),
            "from the start to its end"
        );
        assert_eq!(
            row_selection(&resolved, 2, 10),
            Some(0..10),
            "a middle row, whole"
        );
        assert_eq!(row_selection(&resolved, 3, 10), Some(0..2), "up to the end");
        assert_eq!(row_selection(&resolved, 4, 10), None, "below the selection");
        // The offsets are clamped to a message that has become shorter.
        assert_eq!(row_selection(&resolved, 1, 3), None);
        let single = Resolved {
            first_row: 2,
            first_offset: 3,
            last_row: 2,
            last_offset: 7,
        };
        assert_eq!(row_selection(&single, 2, 20), Some(3..7));
        assert_eq!(row_selection(&single, 1, 20), None);
    }

    #[test]
    fn snapping_never_splits_a_grapheme_cluster() {
        // "e" + combining acute (3 bytes), a family emoji (ZWJ sequence), a flag.
        let text = "e\u{301}x👨‍👩‍👧y🇯🇵z";
        let boundaries: Vec<usize> = text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .chain([text.len()])
            .collect();
        for offset in 0..=text.len() {
            if !text.is_char_boundary(offset) {
                continue;
            }
            let snapped = snap(text, offset);
            assert!(boundaries.contains(&snapped), "{offset} -> {snapped}");
            if boundaries.contains(&offset) {
                assert_eq!(snapped, offset, "a boundary stays where it is");
            }
        }
        // Inside the family emoji, the nearer edge wins.
        let family = "a👨‍👩‍👧b";
        let start = 1;
        let end = family.len() - 1;
        assert_eq!(snap(family, start + 4), start);
        assert_eq!(snap(family, end - 4), end);
        assert_eq!(
            snap("é", 1),
            0,
            "inside a two-byte letter is not a boundary"
        );
        assert_eq!(snap("abc", 99), 3);
    }

    #[test]
    fn a_double_click_selects_a_word_in_text_of_every_kind() {
        let text = "hello wörld, 你好 שלום!";
        let word = |at: usize| &text[word_at(text, at)];
        assert_eq!(word(1), "hello");
        assert_eq!(word(8), "wörld");
        assert_eq!(word(5), " ", "on the space it is the space");
        assert_eq!(word(0), "hello");
        // Hebrew is a word like any other, in logical order.
        let hebrew = text.find('ש').unwrap();
        assert_eq!(word(hebrew + 2), "שלום");
        // Each ideograph is its own word under the Unicode rules.
        let cjk = text.find('你').unwrap();
        assert_eq!(word(cjk), "你");
        assert_eq!(word(cjk + 3), "好");
        // At the very end of the text, the last word.
        assert_eq!(&text[word_at(text, text.len())], "!");
        assert_eq!(word_at("", 0), 0..0);
    }

    fn rows(index: usize) -> Option<CopyRow> {
        let table = [
            ("Alice", "3:41 PM", "First message here"),
            ("Bob", "3:42 PM", "Second, with 👨‍👩‍👧 and e\u{301}"),
            ("Alice", "3:44 PM", "Third one"),
        ];
        table.get(index).map(|(author, time, text)| CopyRow {
            author: (*author).to_owned(),
            time: (*time).to_owned(),
            text: (*text).to_owned(),
        })
    }

    #[test]
    fn a_copy_inside_one_message_is_just_the_selected_text() {
        let resolved = Resolved {
            first_row: 0,
            first_offset: 6,
            last_row: 0,
            last_offset: 13,
        };
        assert_eq!(copy_text(&resolved, rows), "message");
    }

    #[test]
    fn a_copy_across_messages_names_each_author_and_time_and_separates_them_with_a_blank_line() {
        let resolved = Resolved {
            first_row: 0,
            first_offset: 6,
            last_row: 2,
            last_offset: 5,
        };
        assert_eq!(
            copy_text(&resolved, rows),
            "Alice  3:41 PM\nmessage here\n\n\
             Bob  3:42 PM\nSecond, with 👨‍👩‍👧 and e\u{301}\n\n\
             Alice  3:44 PM\nThird"
        );
    }

    #[test]
    fn a_copy_that_only_touches_the_edge_of_a_message_leaves_it_out() {
        let first = "First message here".len();
        let resolved = Resolved {
            first_row: 0,
            first_offset: first,
            last_row: 2,
            last_offset: 0,
        };
        assert_eq!(
            copy_text(&resolved, rows),
            "Bob  3:42 PM\nSecond, with 👨‍👩‍👧 and e\u{301}"
        );
    }

    #[test]
    fn an_empty_selection_copies_nothing_and_a_copy_never_splits_a_cluster() {
        let nothing = Resolved {
            first_row: 1,
            first_offset: 4,
            last_row: 1,
            last_offset: 4,
        };
        assert!(nothing.is_empty());
        assert_eq!(copy_text(&nothing, rows), "");
        // Offsets that land inside the family emoji are moved to its edges.
        let text = rows(1).unwrap().text;
        let family = text.find('👨').unwrap();
        let resolved = Resolved {
            first_row: 1,
            first_offset: family + 2,
            last_row: 1,
            last_offset: text.len(),
        };
        let copied = copy_text(&resolved, rows);
        assert!(
            copied.starts_with(" and") || copied.starts_with("👨"),
            "{copied:?}"
        );
        assert!(copied.ends_with("e\u{301}"));
    }
}
