//! The pure editing model behind [`TextInput`](super::TextInput).
//!
//! `EditorModel` owns the text, the selection, the IME composition ("marked") range, and the undo
//! history. It knows nothing about windows, fonts, or layout, so every rule here is testable
//! without GPUI. Visual-row movement (up/down across soft-wrapped rows) needs layout and lives
//! in the view, which feeds the resulting offset back through [`EditorModel::move_to`].
//!
//! # Units
//!
//! All offsets are UTF-8 byte offsets. The selection always sits on grapheme-cluster boundaries,
//! except while a composition is active: an IME may select inside its own preedit text, which
//! can end mid-cluster. Methods suffixed `_utf16` speak the platform text-input contract, whose
//! unit is UTF-16 code units.

use std::ops::Range;

use super::unicode::{
    Rounding, ceil_char_boundary, floor_char_boundary, line_end, line_start,
    next_grapheme_boundary, next_word_boundary, normalize_input, offset_from_utf16,
    offset_to_utf16, prev_grapheme_boundary, prev_word_boundary, snap_down, snap_up,
    truncate_to_boundary, utf16_len, word_range_at,
};

/// Default capacity: the longest message document the domain accepts.
pub const DEFAULT_MAX_BYTES: usize = lazy_domain::document::MAX_TEXT_BYTES;

/// Most edits kept in the undo history. Older groups are discarded first.
const MAX_UNDO_EDITS: usize = 2_000;
/// Longest run of single-character typing edits merged into one undo step.
const MAX_TYPING_GROUP: usize = 64;

/// An anchor and a head. The head is the end that moves when the selection is extended, so the
/// selection is "reversed" when the head precedes the anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub const fn caret(offset: usize) -> Self {
        Self {
            anchor: offset,
            head: offset,
        }
    }

    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    pub fn is_reversed(&self) -> bool {
        self.head < self.anchor
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Backward,
    Forward,
}

/// How far a movement or deletion reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// One grapheme cluster.
    Grapheme,
    /// To the previous or next word boundary.
    Word,
    /// To the start or end of the logical line (the text between newlines).
    Line,
    /// To the start or end of the text.
    Document,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Typing,
    Newline,
    Paste,
    Delete,
    DeleteBackward,
    DeleteForward,
    Composition,
}

/// One reversible replacement. Edits that share a `group` undo and redo together.
#[derive(Debug, Clone)]
struct Edit {
    at: usize,
    removed: String,
    inserted: String,
    before: Selection,
    after: Selection,
    kind: EditKind,
    group: u64,
}

struct Replaced {
    inserted: Range<usize>,
    text_changed: bool,
}

#[derive(Debug, Clone)]
pub struct EditorModel {
    text: String,
    selection: Selection,
    marked: Option<Range<usize>>,
    composition_group: Option<u64>,
    max_bytes: usize,
    single_line: bool,
    revision: u64,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    next_group: u64,
}

impl EditorModel {
    /// Creates an empty model holding at most `max_bytes` bytes of text. In a `single_line`
    /// model, line breaks in incoming text become spaces.
    pub fn new(max_bytes: usize, single_line: bool) -> Self {
        Self {
            text: String::new(),
            selection: Selection::caret(0),
            marked: None,
            composition_group: None,
            max_bytes,
            single_line,
            revision: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            next_group: 0,
        }
    }

    // ---- Reading -----------------------------------------------------------------------------

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Increases whenever the text or the marked range changes. Cache layout by this value.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    pub fn selection(&self) -> Selection {
        self.selection
    }

    pub fn selected_range(&self) -> Range<usize> {
        self.selection.range()
    }

    pub fn selected_text(&self) -> &str {
        &self.text[self.selection.range()]
    }

    /// The moving end of the selection.
    pub fn cursor(&self) -> usize {
        self.selection.head
    }

    pub fn marked_range(&self) -> Option<Range<usize>> {
        self.marked.clone()
    }

    /// Whether an IME composition is in progress.
    pub fn is_composing(&self) -> bool {
        self.marked.is_some()
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty() && !self.is_composing()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty() && !self.is_composing()
    }

    // ---- Programmatic replacement ------------------------------------------------------------

    /// Replaces everything. Clears history and any composition and puts the caret at the end.
    /// Over-long text is truncated on a grapheme boundary.
    pub fn set_text(&mut self, text: &str) {
        let normalized = normalize_input(text, self.single_line);
        self.text = truncate_to_boundary(&normalized, self.max_bytes).to_owned();
        self.selection = Selection::caret(self.text.len());
        self.marked = None;
        self.composition_group = None;
        self.undo.clear();
        self.redo.clear();
        self.revision += 1;
    }

    // ---- Typing and editing ------------------------------------------------------------------

    /// Types `text`, replacing the selection. Ignored while composing, because the IME owns the
    /// text then. Returns whether the text changed.
    pub fn insert(&mut self, text: &str) -> bool {
        self.replace_selection(text, EditKind::Typing)
    }

    /// Inserts a line break, replacing the selection. Does nothing in a single-line model.
    pub fn insert_newline(&mut self) -> bool {
        if self.single_line {
            return false;
        }
        self.replace_selection("\n", EditKind::Newline)
    }

    /// Pastes `text`, replacing the selection, as one undo step.
    pub fn paste(&mut self, text: &str) -> bool {
        self.replace_selection(text, EditKind::Paste)
    }

    /// Deletes the selection as one undo step (used by cut).
    pub fn delete_selection(&mut self) -> bool {
        if self.is_composing() || self.selection.is_empty() {
            return false;
        }
        self.replace_selection("", EditKind::Delete)
    }

    /// Deletes the selection, or if it is empty the text between the caret and the target of
    /// `unit` in `direction`. Returns whether anything was deleted.
    pub fn delete(&mut self, direction: Direction, unit: Unit) -> bool {
        if self.is_composing() {
            return false;
        }
        if !self.selection.is_empty() {
            return self.replace_selection("", EditKind::Delete);
        }
        let head = self.selection.head;
        let target = self.target(head, direction, unit);
        if target == head {
            return false;
        }
        let kind = match direction {
            Direction::Backward => EditKind::DeleteBackward,
            Direction::Forward => EditKind::DeleteForward,
        };
        let range = target.min(head)..target.max(head);
        match self.replace(range, "", kind, None) {
            Some(replaced) => {
                self.selection = Selection::caret(snap_up(&self.text, replaced.inserted.start));
                replaced.text_changed
            }
            None => false,
        }
    }

    fn replace_selection(&mut self, text: &str, kind: EditKind) -> bool {
        if self.is_composing() {
            return false;
        }
        let range = self.selection.range();
        match self.replace(range, text, kind, None) {
            Some(replaced) => {
                self.selection = Selection::caret(snap_up(&self.text, replaced.inserted.end));
                replaced.text_changed
            }
            None => false,
        }
    }

    /// Replaces `range` with `text`, enforcing normalization and the length bound, and records
    /// the edit. The selection is left for the caller to place. `None` means there was nothing
    /// to do: an empty range and nothing to insert.
    fn replace(
        &mut self,
        range: Range<usize>,
        text: &str,
        kind: EditKind,
        forced_group: Option<u64>,
    ) -> Option<Replaced> {
        let start = floor_char_boundary(&self.text, range.start);
        let end = ceil_char_boundary(&self.text, range.end).max(start);
        let normalized = normalize_input(text, self.single_line);
        let allowed = self
            .max_bytes
            .saturating_sub(self.text.len() - (end - start));
        let insert = truncate_to_boundary(&normalized, allowed);
        if start == end && insert.is_empty() {
            return None;
        }
        let inserted = start..start + insert.len();
        if self.text[start..end] == *insert {
            return Some(Replaced {
                inserted,
                text_changed: false,
            });
        }

        let removed = self.text[start..end].to_owned();
        let before = self.selection;
        self.text.replace_range(start..end, insert);
        let after = Selection::caret(snap_up(&self.text, inserted.end));
        let group = forced_group
            .unwrap_or_else(|| self.group_for(kind, start, removed.len(), insert.len(), before));
        self.undo.push(Edit {
            at: start,
            removed,
            inserted: insert.to_owned(),
            before,
            after,
            kind,
            group,
        });
        self.trim_history();
        self.redo.clear();
        self.revision += 1;
        Some(Replaced {
            inserted,
            text_changed: true,
        })
    }

    /// Picks the undo group for a new edit: it joins the previous group when it plainly
    /// continues it (typing the next character, repeating backspace, repeating delete).
    fn group_for(
        &mut self,
        kind: EditKind,
        at: usize,
        removed_len: usize,
        inserted_len: usize,
        before: Selection,
    ) -> u64 {
        if let Some(last) = self.undo.last() {
            let run = self
                .undo
                .iter()
                .rev()
                .take_while(|e| e.group == last.group)
                .count();
            let continues = last.kind == kind
                && before == last.after
                && run < MAX_TYPING_GROUP
                && match kind {
                    EditKind::Typing => removed_len == 0 && at == last.at + last.inserted.len(),
                    EditKind::DeleteBackward => inserted_len == 0 && at + removed_len == last.at,
                    EditKind::DeleteForward => inserted_len == 0 && at == last.at,
                    _ => false,
                };
            if continues {
                return last.group;
            }
        }
        self.fresh_group()
    }

    fn fresh_group(&mut self) -> u64 {
        self.next_group += 1;
        self.next_group
    }

    fn trim_history(&mut self) {
        while self.undo.len() > MAX_UNDO_EDITS {
            let group = self.undo[0].group;
            let keep_from = self
                .undo
                .iter()
                .position(|e| e.group != group)
                .unwrap_or(self.undo.len());
            self.undo.drain(..keep_from);
        }
    }

    // ---- Undo and redo -----------------------------------------------------------------------

    /// Reverses the most recent group of edits. Returns false when there is nothing to undo or
    /// a composition is active.
    pub fn undo(&mut self) -> bool {
        if !self.can_undo() {
            return false;
        }
        let group = self.undo.last().map(|e| e.group).unwrap_or_default();
        let mut selection = self.selection;
        while self.undo.last().is_some_and(|e| e.group == group) {
            let Some(edit) = self.undo.pop() else { break };
            let end = edit.at + edit.inserted.len();
            self.text.replace_range(edit.at..end, &edit.removed);
            selection = edit.before;
            self.redo.push(edit);
        }
        self.selection = selection;
        self.revision += 1;
        true
    }

    /// Reapplies the most recently undone group.
    pub fn redo(&mut self) -> bool {
        if !self.can_redo() {
            return false;
        }
        let group = self.redo.last().map(|e| e.group).unwrap_or_default();
        let mut selection = self.selection;
        while self.redo.last().is_some_and(|e| e.group == group) {
            let Some(edit) = self.redo.pop() else { break };
            let end = edit.at + edit.removed.len();
            self.text.replace_range(edit.at..end, &edit.inserted);
            selection = edit.after;
            self.undo.push(edit);
        }
        self.selection = selection;
        self.revision += 1;
        true
    }

    // ---- Selection and movement --------------------------------------------------------------

    /// Moves the caret to `offset` (snapped to a grapheme boundary). With `extend`, only the
    /// head moves and the anchor stays. Returns whether the selection changed.
    pub fn move_to(&mut self, offset: usize, extend: bool) -> bool {
        let head = snap_down(&self.text, offset);
        let next = if extend {
            Selection {
                anchor: self.selection.anchor,
                head,
            }
        } else {
            Selection::caret(head)
        };
        self.set_selection(next)
    }

    /// Moves the head by `unit` in `direction`. Without `extend`, a non-empty selection first
    /// collapses to its near edge when moving by grapheme.
    pub fn move_cursor(&mut self, direction: Direction, unit: Unit, extend: bool) -> bool {
        if !extend && !self.selection.is_empty() && unit == Unit::Grapheme {
            let range = self.selection.range();
            let edge = match direction {
                Direction::Backward => range.start,
                Direction::Forward => range.end,
            };
            return self.set_selection(Selection::caret(edge));
        }
        let target = self.target(self.selection.head, direction, unit);
        self.move_to(target, extend)
    }

    pub fn select_all(&mut self) -> bool {
        self.set_selection(Selection {
            anchor: 0,
            head: self.text.len(),
        })
    }

    /// Selects the word under `offset`, as a double-click does.
    pub fn select_word_at(&mut self, offset: usize) -> bool {
        let range = word_range_at(&self.text, offset);
        self.set_selection(Selection {
            anchor: range.start,
            head: range.end,
        })
    }

    /// Selects the logical line under `offset`, as a triple-click does.
    pub fn select_line_at(&mut self, offset: usize) -> bool {
        let start = line_start(&self.text, offset);
        let end = line_end(&self.text, offset);
        self.set_selection(Selection {
            anchor: start,
            head: end,
        })
    }

    /// Sets the selection, snapping both ends to grapheme boundaries (outward for a non-empty
    /// selection). Returns whether it changed.
    pub fn select(&mut self, anchor: usize, head: usize) -> bool {
        let selection = self.snapped(anchor, head);
        self.set_selection(selection)
    }

    /// `anchor..head` with both ends moved to grapheme boundaries (outward for a non-empty
    /// range), keeping which end is the head.
    fn snapped(&self, anchor: usize, head: usize) -> Selection {
        let (low, high) = (anchor.min(head), anchor.max(head));
        let (low, high) = if low == high {
            let caret = snap_down(&self.text, low);
            (caret, caret)
        } else {
            (snap_down(&self.text, low), snap_up(&self.text, high))
        };
        if head < anchor {
            Selection {
                anchor: high,
                head: low,
            }
        } else {
            Selection {
                anchor: low,
                head: high,
            }
        }
    }

    fn set_selection(&mut self, selection: Selection) -> bool {
        if selection == self.selection {
            return false;
        }
        self.selection = selection;
        true
    }

    fn target(&self, from: usize, direction: Direction, unit: Unit) -> usize {
        let text = &self.text;
        match (direction, unit) {
            (Direction::Backward, Unit::Grapheme) => prev_grapheme_boundary(text, from),
            (Direction::Forward, Unit::Grapheme) => next_grapheme_boundary(text, from),
            (Direction::Backward, Unit::Word) => prev_word_boundary(text, from),
            (Direction::Forward, Unit::Word) => next_word_boundary(text, from),
            (Direction::Backward, Unit::Line) => {
                let start = line_start(text, from);
                if start == from {
                    prev_grapheme_boundary(text, from)
                } else {
                    start
                }
            }
            (Direction::Forward, Unit::Line) => {
                let end = line_end(text, from);
                if end == from {
                    next_grapheme_boundary(text, from)
                } else {
                    end
                }
            }
            (Direction::Backward, Unit::Document) => 0,
            (Direction::Forward, Unit::Document) => text.len(),
        }
    }

    // ---- Platform text-input contract (UTF-16) -----------------------------------------------

    pub fn len_utf16(&self) -> usize {
        utf16_len(&self.text)
    }

    pub fn offset_to_utf16(&self, offset: usize) -> usize {
        offset_to_utf16(&self.text, offset)
    }

    /// A UTF-16 range as bytes: the start rounds down and the end rounds up, so a range that
    /// touches half of a surrogate pair covers the whole character.
    pub fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        let start = offset_from_utf16(&self.text, range.start, Rounding::Down);
        let end = offset_from_utf16(&self.text, range.end, Rounding::Up).max(start);
        start..end
    }

    pub fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    pub fn selection_utf16(&self) -> (Range<usize>, bool) {
        (
            self.range_to_utf16(&self.selection.range()),
            self.selection.is_reversed(),
        )
    }

    pub fn marked_range_utf16(&self) -> Option<Range<usize>> {
        self.marked.as_ref().map(|range| self.range_to_utf16(range))
    }

    /// The text of a UTF-16 range, clamped to the document, with the range actually covered.
    pub fn text_for_range_utf16(&self, range: &Range<usize>) -> (String, Range<usize>) {
        let bytes = self.range_from_utf16(range);
        (
            self.text[bytes.clone()].to_owned(),
            self.range_to_utf16(&bytes),
        )
    }

    /// Applies a selection requested by the platform (UTF-16), snapped to grapheme boundaries.
    pub fn set_selection_utf16(&mut self, range: &Range<usize>) -> bool {
        let bytes = self.range_from_utf16(range);
        self.select(bytes.start, bytes.end)
    }

    /// Platform `insertText`: commits `text` over `range`, or over the marked range, or over the
    /// selection. Ends any composition. Returns whether the text changed.
    pub fn replace_text_in_range_utf16(&mut self, range: Option<Range<usize>>, text: &str) -> bool {
        let target = match range {
            Some(range) => self.range_from_utf16(&range),
            None => self
                .marked
                .clone()
                .unwrap_or_else(|| self.selection.range()),
        };
        let group = self.composition_group.take();
        let marked_before = self.marked.take();
        let kind = if group.is_some() {
            EditKind::Composition
        } else {
            EditKind::Typing
        };
        let outcome = self.replace(target.clone(), text, kind, group);
        let changed = match &outcome {
            Some(replaced) => {
                self.selection = Selection::caret(snap_up(&self.text, replaced.inserted.end));
                replaced.text_changed
            }
            None => {
                self.selection = Selection::caret(snap_up(&self.text, target.start));
                false
            }
        };
        if !changed && marked_before.is_some() {
            self.revision += 1;
        }
        changed
    }

    /// Platform `setMarkedText`: replaces `range` (or the marked range, or the selection) with
    /// preedit `text`, marks it as the composition, and selects `selected` (UTF-16, relative to
    /// the start of `text`). Empty text cancels the composition. All edits made during one
    /// composition undo together.
    pub fn replace_and_mark_utf16(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
    ) -> bool {
        let target = match range {
            Some(range) => self.range_from_utf16(&range),
            None => self
                .marked
                .clone()
                .unwrap_or_else(|| self.selection.range()),
        };
        let marked_before = self.marked.clone();
        let group = match self.composition_group {
            Some(group) => group,
            None => {
                let group = self.fresh_group();
                self.composition_group = Some(group);
                group
            }
        };
        let outcome = self.replace(target.clone(), text, EditKind::Composition, Some(group));
        let (inserted, changed) = match outcome {
            Some(replaced) => (replaced.inserted, replaced.text_changed),
            None => (target.start..target.start, false),
        };

        if inserted.is_empty() {
            self.marked = None;
            self.composition_group = None;
            self.selection = Selection::caret(snap_up(&self.text, inserted.start));
        } else {
            self.marked = Some(inserted.clone());
            self.selection = match selected {
                Some(selected) => {
                    let start16 = offset_to_utf16(&self.text, inserted.start);
                    let length16 = utf16_len(&self.text[inserted.clone()]);
                    let low = start16 + selected.start.min(length16);
                    let high = start16 + selected.end.max(selected.start).min(length16);
                    Selection {
                        anchor: offset_from_utf16(&self.text, low, Rounding::Down),
                        head: offset_from_utf16(&self.text, high, Rounding::Up),
                    }
                }
                None => Selection::caret(inserted.end),
            };
        }
        if !changed && self.marked != marked_before {
            self.revision += 1;
        }
        changed
    }

    /// Platform `unmarkText`: keeps the preedit text as ordinary text. Returns whether a
    /// composition was active.
    pub fn unmark(&mut self) -> bool {
        self.composition_group = None;
        if self.marked.take().is_some() {
            // The IME may have left the selection inside its preedit text, mid-cluster.
            self.selection = self.snapped(self.selection.anchor, self.selection.head);
            self.revision += 1;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAMILY: &str = "👨‍👩‍👧‍👦";
    const FLAG: &str = "🇨🇦";

    fn model(text: &str) -> EditorModel {
        let mut m = EditorModel::new(DEFAULT_MAX_BYTES, false);
        m.set_text(text);
        m
    }

    #[test]
    fn set_text_places_caret_at_end_and_clears_history() {
        let mut m = model("");
        m.insert("a");
        m.set_text("hello");
        assert_eq!(m.selection(), Selection::caret(5));
        assert!(!m.can_undo());
    }

    #[test]
    fn typing_replaces_the_selection() {
        let mut m = model("hello world");
        m.select(0, 5);
        assert!(m.insert("bye"));
        assert_eq!(m.text(), "bye world");
        assert_eq!(m.selection(), Selection::caret(3));
    }

    #[test]
    fn grapheme_movement_treats_clusters_as_one_character() {
        let text = format!("a{FAMILY}{FLAG}e\u{301}z");
        let mut m = model(&text);
        m.move_to(0, false);
        let mut stops = vec![0];
        while m.move_cursor(Direction::Forward, Unit::Grapheme, false) {
            stops.push(m.cursor());
        }
        assert_eq!(
            stops,
            vec![
                0,
                1,
                1 + FAMILY.len(),
                1 + FAMILY.len() + FLAG.len(),
                1 + FAMILY.len() + FLAG.len() + 3,
                text.len()
            ]
        );
        for expected in stops.iter().rev().skip(1) {
            assert!(m.move_cursor(Direction::Backward, Unit::Grapheme, false));
            assert_eq!(m.cursor(), *expected);
        }
    }

    #[test]
    fn backspace_removes_a_whole_cluster() {
        let mut m = model(&format!("a{FAMILY}"));
        assert!(m.delete(Direction::Backward, Unit::Grapheme));
        assert_eq!(m.text(), "a");
        let mut m = model("xe\u{301}");
        assert!(m.delete(Direction::Backward, Unit::Grapheme));
        assert_eq!(m.text(), "x");
        let mut m = model(&format!("{FLAG}{FLAG}"));
        assert!(m.delete(Direction::Backward, Unit::Grapheme));
        assert_eq!(m.text(), FLAG);
    }

    #[test]
    fn delete_forward_removes_the_next_cluster() {
        let mut m = model(&format!("{FAMILY}b"));
        m.move_to(0, false);
        assert!(m.delete(Direction::Forward, Unit::Grapheme));
        assert_eq!(m.text(), "b");
        assert!(!{
            let mut empty = model("");
            empty.delete(Direction::Forward, Unit::Grapheme)
        });
    }

    #[test]
    fn collapsing_a_selection_goes_to_the_near_edge() {
        let mut m = model("abcdef");
        m.select(4, 1);
        assert!(m.selection().is_reversed());
        m.move_cursor(Direction::Forward, Unit::Grapheme, false);
        assert_eq!(m.selection(), Selection::caret(4));
        m.select(1, 4);
        m.move_cursor(Direction::Backward, Unit::Grapheme, false);
        assert_eq!(m.selection(), Selection::caret(1));
    }

    #[test]
    fn extending_keeps_the_anchor_and_can_reverse() {
        let mut m = model("abcdef");
        m.move_to(3, false);
        m.move_cursor(Direction::Forward, Unit::Grapheme, true);
        assert_eq!(m.selection(), Selection { anchor: 3, head: 4 });
        m.move_to(1, true);
        assert_eq!(m.selection(), Selection { anchor: 3, head: 1 });
        assert!(m.selection().is_reversed());
        assert_eq!(m.selected_range(), 1..3);
    }

    #[test]
    fn word_movement_and_deletion() {
        let mut m = model("hello, big 🎉🎉 world");
        m.move_to(0, false);
        m.move_cursor(Direction::Forward, Unit::Word, false);
        assert_eq!(m.cursor(), 5);
        m.move_cursor(Direction::Forward, Unit::Word, false);
        assert_eq!(m.cursor(), 6);
        m.move_cursor(Direction::Forward, Unit::Word, false);
        assert_eq!(m.cursor(), 10);
        m.move_cursor(Direction::Forward, Unit::Word, false);
        assert_eq!(m.cursor(), 10 + 1 + 8);
        let end = m.len();
        m.move_to(end, false);
        assert!(m.delete(Direction::Backward, Unit::Word));
        assert_eq!(m.text(), "hello, big 🎉🎉 ");
        assert!(m.delete(Direction::Backward, Unit::Word));
        assert_eq!(m.text(), "hello, big ");
    }

    #[test]
    fn line_and_document_movement() {
        let mut m = model("ab\ncd\nef");
        m.move_to(4, false);
        m.move_cursor(Direction::Backward, Unit::Line, false);
        assert_eq!(m.cursor(), 3);
        m.move_cursor(Direction::Forward, Unit::Line, false);
        assert_eq!(m.cursor(), 5);
        m.move_cursor(Direction::Backward, Unit::Document, false);
        assert_eq!(m.cursor(), 0);
        m.move_cursor(Direction::Forward, Unit::Document, true);
        assert_eq!(m.selection(), Selection { anchor: 0, head: 8 });
        m.move_to(3, false);
        assert!(m.delete(Direction::Backward, Unit::Line));
        assert_eq!(m.text(), "abcd\nef");
    }

    #[test]
    fn double_and_triple_click_selection() {
        let mut m = model("one two\nthree");
        m.select_word_at(5);
        assert_eq!(m.selected_text(), "two");
        m.select_line_at(9);
        assert_eq!(m.selected_text(), "three");
        m.select_line_at(2);
        assert_eq!(m.selected_text(), "one two");
        m.select_all();
        assert_eq!(m.selected_range(), 0..m.len());
    }

    #[test]
    fn selections_snap_to_grapheme_boundaries() {
        let text = format!("a{FAMILY}b");
        let mut m = model(&text);
        m.select(3, 3);
        assert_eq!(m.selection(), Selection::caret(1));
        m.select(2, 6);
        assert_eq!(m.selected_range(), 1..1 + FAMILY.len());
        m.select(6, 2);
        assert!(m.selection().is_reversed());
        assert_eq!(m.selected_range(), 1..1 + FAMILY.len());
    }

    #[test]
    fn paste_normalizes_line_endings_and_controls() {
        let mut m = model("");
        assert!(m.paste("a\r\nb\rc\u{7}\nd"));
        assert_eq!(m.text(), "a\nb\nc\nd");
        let mut single = EditorModel::new(DEFAULT_MAX_BYTES, true);
        assert!(single.paste("a\r\nb\nc"));
        assert_eq!(single.text(), "a b c");
        assert!(!single.insert_newline());
    }

    #[test]
    fn max_length_truncates_on_a_grapheme_boundary() {
        let mut m = EditorModel::new(10, false);
        m.insert("abcdef");
        assert!(!m.insert(&format!("{FAMILY}x")));
        assert_eq!(m.text(), "abcdef");
        m.set_text("");
        m.insert("ab");
        m.insert(&format!("{FLAG}{FLAG}"));
        assert_eq!(m.text(), format!("ab{FLAG}"));
        assert!(m.len() <= 10);
        let mut full = EditorModel::new(3, false);
        full.insert("abc");
        assert!(!full.insert("d"));
        assert_eq!(full.text(), "abc");
        full.select(0, 3);
        assert!(full.insert("xyz!"));
        assert_eq!(full.text(), "xyz");
    }

    #[test]
    fn set_text_truncates_too() {
        let mut m = EditorModel::new(4, false);
        m.set_text(&format!("ab{FAMILY}"));
        assert_eq!(m.text(), "ab");
    }

    #[test]
    fn typing_runs_undo_as_one_step_and_redo() {
        let mut m = model("");
        for c in ["h", "e", "l", "l", "o"] {
            m.insert(c);
        }
        m.insert_newline();
        m.insert("x");
        assert_eq!(m.text(), "hello\nx");
        assert!(m.undo());
        assert_eq!(m.text(), "hello\n");
        assert!(m.undo());
        assert_eq!(m.text(), "hello");
        assert!(m.undo());
        assert_eq!(m.text(), "");
        assert!(!m.undo());
        assert!(m.redo());
        assert_eq!(m.text(), "hello");
        assert!(m.redo());
        assert!(m.redo());
        assert_eq!(m.text(), "hello\nx");
        assert_eq!(m.selection(), Selection::caret(7));
        assert!(!m.redo());
    }

    #[test]
    fn moving_the_caret_breaks_an_undo_group() {
        let mut m = model("");
        m.insert("ab");
        m.move_to(0, false);
        m.insert("c");
        assert_eq!(m.text(), "cab");
        m.undo();
        assert_eq!(m.text(), "ab");
        assert_eq!(m.selection(), Selection::caret(0));
    }

    #[test]
    fn repeated_backspace_is_one_step() {
        let mut m = model("hello");
        for _ in 0..3 {
            m.delete(Direction::Backward, Unit::Grapheme);
        }
        assert_eq!(m.text(), "he");
        assert!(m.undo());
        assert_eq!(m.text(), "hello");
        assert_eq!(m.selection(), Selection::caret(5));
    }

    #[test]
    fn a_new_edit_clears_redo() {
        let mut m = model("");
        m.insert("a");
        m.undo();
        m.insert("b");
        assert!(!m.redo());
        assert_eq!(m.text(), "b");
    }

    #[test]
    fn paste_and_cut_are_single_undo_steps() {
        let mut m = model("abc");
        m.select(0, 3);
        assert!(m.paste("xyz"));
        assert!(m.undo());
        assert_eq!(m.text(), "abc");
        assert_eq!(m.selected_range(), 0..3);
        m.select(1, 2);
        assert!(m.delete_selection());
        assert_eq!(m.text(), "ac");
        m.undo();
        assert_eq!(m.text(), "abc");
    }

    #[test]
    fn utf16_ranges_round_trip_at_every_boundary() {
        let text = format!("a漢{FAMILY}é🎉z");
        let m = model(&text);
        let mut offsets: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
        offsets.push(text.len());
        for &start in &offsets {
            for &end in offsets.iter().filter(|&&e| e >= start) {
                let units = m.range_to_utf16(&(start..end));
                assert_eq!(m.range_from_utf16(&units), start..end);
            }
        }
        assert_eq!(m.len_utf16(), text.encode_utf16().count());
    }

    #[test]
    fn text_for_range_covers_whole_characters() {
        let m = model("a🎉b");
        let (text, actual) = m.text_for_range_utf16(&(2..3));
        assert_eq!(text, "🎉");
        assert_eq!(actual, 1..3);
        let (text, actual) = m.text_for_range_utf16(&(0..99));
        assert_eq!(text, "a🎉b");
        assert_eq!(actual, 0..4);
    }

    #[test]
    fn platform_selection_is_utf16_and_snapped() {
        let mut m = model(&format!("e{FAMILY}"));
        assert!(m.set_selection_utf16(&(2..3)));
        assert_eq!(m.selected_range(), 1..1 + FAMILY.len());
        let (range, reversed) = m.selection_utf16();
        assert_eq!(range, 1..1 + FAMILY.encode_utf16().count());
        assert!(!reversed);
    }

    #[test]
    fn ime_composition_flow_commit() {
        let mut m = model("ab");
        m.move_to(1, false);
        assert!(m.replace_and_mark_utf16(None, "に", Some(1..1)));
        assert_eq!(m.text(), "aにb");
        assert_eq!(m.marked_range(), Some(1..4));
        assert!(m.is_composing());
        assert_eq!(m.selection(), Selection::caret(4));
        assert!(m.replace_and_mark_utf16(None, "日本", Some(2..2)));
        assert_eq!(m.text(), "a日本b");
        assert_eq!(m.marked_range(), Some(1..7));
        assert_eq!(m.marked_range_utf16(), Some(1..3));
        m.replace_text_in_range_utf16(None, "日本");
        assert_eq!(m.text(), "a日本b");
        assert!(!m.is_composing());
        assert_eq!(m.selection(), Selection::caret(7));
    }

    #[test]
    fn ime_composition_undoes_as_one_step() {
        let mut m = model("");
        m.insert("x");
        m.replace_and_mark_utf16(None, "に", None);
        m.replace_and_mark_utf16(None, "日", None);
        m.replace_and_mark_utf16(None, "日本", None);
        m.replace_text_in_range_utf16(None, "日本");
        assert_eq!(m.text(), "x日本");
        assert!(m.undo());
        assert_eq!(m.text(), "x");
        assert!(m.redo());
        assert_eq!(m.text(), "x日本");
    }

    #[test]
    fn ime_composition_cancel_removes_preedit_text() {
        let mut m = model("ab");
        m.move_to(1, false);
        m.replace_and_mark_utf16(None, "き", None);
        assert_eq!(m.text(), "aきb");
        assert!(m.replace_and_mark_utf16(None, "", None));
        assert_eq!(m.text(), "ab");
        assert!(!m.is_composing());
        assert_eq!(m.selection(), Selection::caret(1));
        assert!(m.undo());
        assert_eq!(m.text(), "ab");
    }

    #[test]
    fn ime_selection_inside_preedit_is_relative_and_clamped() {
        let mut m = model("A🎉Z");
        m.replace_and_mark_utf16(Some(1..3), "é🎉", Some(1..3));
        assert_eq!(m.text(), "Aé🎉Z");
        assert_eq!(m.marked_range(), Some(1..7));
        assert_eq!(m.selected_range(), 3..7);
        m.replace_and_mark_utf16(None, "漢", Some(1..1));
        assert_eq!(m.text(), "A漢Z");
        assert_eq!(m.marked_range(), Some(1..4));
        assert_eq!(m.selection(), Selection::caret(4));
        m.replace_and_mark_utf16(None, "", Some(0..0));
        assert_eq!(m.text(), "AZ");
        assert_eq!(m.marked_range(), None);
        assert_eq!(m.selection(), Selection::caret(1));
    }

    #[test]
    fn unmark_keeps_the_text() {
        let mut m = model("");
        m.replace_and_mark_utf16(None, "かな", None);
        assert!(m.unmark());
        assert_eq!(m.text(), "かな");
        assert!(!m.is_composing());
        assert!(!m.unmark());
    }

    #[test]
    fn typing_and_deleting_are_ignored_while_composing() {
        let mut m = model("");
        m.replace_and_mark_utf16(None, "か", None);
        assert!(!m.insert("x"));
        assert!(!m.paste("y"));
        assert!(!m.delete(Direction::Backward, Unit::Grapheme));
        assert!(!m.undo());
        assert_eq!(m.text(), "か");
    }

    #[test]
    fn revision_changes_with_text_and_marked_range() {
        let mut m = model("");
        let r0 = m.revision();
        m.replace_and_mark_utf16(None, "か", None);
        let r1 = m.revision();
        assert!(r1 > r0);
        m.unmark();
        assert!(m.revision() > r1);
        let r2 = m.revision();
        m.move_to(0, false);
        assert_eq!(m.revision(), r2);
    }

    #[test]
    fn committing_identical_text_still_ends_the_composition() {
        let mut m = model("");
        m.replace_and_mark_utf16(None, "ab", None);
        let before = m.revision();
        assert!(!m.replace_text_in_range_utf16(None, "ab"));
        assert!(!m.is_composing());
        assert!(m.revision() > before);
        assert_eq!(m.selection(), Selection::caret(2));
    }

    #[test]
    fn explicit_replacement_range_overrides_the_selection() {
        let mut m = model("hello");
        assert!(m.replace_text_in_range_utf16(Some(1..4), "i"));
        assert_eq!(m.text(), "hio");
        assert_eq!(m.selection(), Selection::caret(2));
    }

    #[test]
    fn history_is_bounded() {
        let mut m = model("");
        for _ in 0..(MAX_UNDO_EDITS + 50) {
            m.insert("a");
        }
        assert!(m.undo.len() <= MAX_UNDO_EDITS);
        assert!(m.undo.len() > MAX_UNDO_EDITS - MAX_TYPING_GROUP);
        let before = m.len();
        assert!(m.undo());
        let undone = before - m.len();
        assert!(
            (1..=MAX_TYPING_GROUP).contains(&undone),
            "undid {undone} edits at once"
        );
    }

    mod properties {
        use proptest::prelude::*;

        use super::*;
        use crate::text_input::unicode::is_grapheme_boundary;

        #[derive(Debug, Clone)]
        enum Op {
            Insert(String),
            Newline,
            Paste(String),
            Delete(Direction, Unit),
            Move(Direction, Unit, bool),
            MoveTo(usize, bool),
            SelectAll,
            SelectWord(usize),
            Mark(String, Option<(usize, usize)>),
            Commit(String),
            Unmark,
            Replace(usize, usize, String),
        }

        fn fragment() -> impl Strategy<Value = String> {
            // Pieces chosen to merge into surprising clusters when concatenated.
            prop::collection::vec(
                prop::sample::select(vec![
                    "a",
                    "é",
                    "e\u{301}",
                    "\u{301}",
                    "🎉",
                    "👨\u{200d}👩",
                    "\u{200d}",
                    "👧",
                    "🇨",
                    "🇦",
                    "🇯",
                    "\n",
                    " ",
                    "漢",
                    "\r\n",
                    "\t",
                    "\u{7}",
                    "word",
                    ".",
                ]),
                0..4,
            )
            .prop_map(|parts| parts.concat())
        }

        fn direction() -> impl Strategy<Value = Direction> {
            prop_oneof![Just(Direction::Backward), Just(Direction::Forward)]
        }

        fn unit() -> impl Strategy<Value = Unit> {
            prop_oneof![
                Just(Unit::Grapheme),
                Just(Unit::Word),
                Just(Unit::Line),
                Just(Unit::Document)
            ]
        }

        fn op() -> impl Strategy<Value = Op> {
            prop_oneof![
                fragment().prop_map(Op::Insert),
                Just(Op::Newline),
                fragment().prop_map(Op::Paste),
                (direction(), unit()).prop_map(|(d, u)| Op::Delete(d, u)),
                (direction(), unit(), any::<bool>()).prop_map(|(d, u, e)| Op::Move(d, u, e)),
                (0usize..40, any::<bool>()).prop_map(|(o, e)| Op::MoveTo(o, e)),
                Just(Op::SelectAll),
                (0usize..40).prop_map(Op::SelectWord),
                (fragment(), prop::option::of((0usize..6, 0usize..6)))
                    .prop_map(|(t, s)| Op::Mark(t, s)),
                fragment().prop_map(Op::Commit),
                Just(Op::Unmark),
                (0usize..30, 0usize..30, fragment()).prop_map(|(a, b, t)| Op::Replace(a, b, t)),
            ]
        }

        fn apply(model: &mut EditorModel, op: &Op) -> bool {
            match op {
                Op::Insert(text) => model.insert(text),
                Op::Newline => model.insert_newline(),
                Op::Paste(text) => model.paste(text),
                Op::Delete(direction, unit) => model.delete(*direction, *unit),
                Op::Move(direction, unit, extend) => model.move_cursor(*direction, *unit, *extend),
                Op::MoveTo(offset, extend) => model.move_to(*offset, *extend),
                Op::SelectAll => model.select_all(),
                Op::SelectWord(offset) => model.select_word_at(*offset),
                Op::Mark(text, selected) => {
                    let selected = selected.map(|(a, b)| a.min(b)..a.max(b));
                    model.replace_and_mark_utf16(None, text, selected)
                }
                Op::Commit(text) => model.replace_text_in_range_utf16(None, text),
                Op::Unmark => model.unmark(),
                Op::Replace(a, b, text) => {
                    model.replace_text_in_range_utf16(Some(*a.min(b)..*a.max(b)), text)
                }
            }
        }

        fn check_invariants(model: &EditorModel) {
            let text = model.text();
            let selection = model.selection();
            for offset in [selection.anchor, selection.head] {
                assert!(
                    offset <= text.len(),
                    "selection {selection:?} past {}",
                    text.len()
                );
                assert!(
                    text.is_char_boundary(offset),
                    "{selection:?} splits a character"
                );
            }
            if let Some(marked) = model.marked_range() {
                assert!(marked.start <= marked.end && marked.end <= text.len());
                assert!(text.is_char_boundary(marked.start) && text.is_char_boundary(marked.end));
            } else {
                for offset in [selection.anchor, selection.head] {
                    assert!(
                        is_grapheme_boundary(text, offset),
                        "{selection:?} splits a grapheme cluster in {text:?}"
                    );
                }
            }
            assert!(text.len() <= model.max_bytes());
            assert!(
                !text
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t')
            );
            assert_eq!(
                model.range_from_utf16(&(0..model.len_utf16())),
                0..text.len(),
                "UTF-16 length matches"
            );
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(400))]

            #[test]
            fn invariants_hold_and_history_round_trips(ops in prop::collection::vec(op(), 1..40)) {
                let mut model = EditorModel::new(300, false);
                let mut last_revision = model.revision();
                for op in &ops {
                    apply(&mut model, op);
                    check_invariants(&model);
                    prop_assert!(model.revision() >= last_revision);
                    last_revision = model.revision();
                }

                model.unmark();
                check_invariants(&model);
                let final_text = model.text().to_owned();

                let mut undone = 0;
                while model.undo() {
                    undone += 1;
                    check_invariants(&model);
                    prop_assert!(undone < 1_000, "undo did not terminate");
                }
                prop_assert_eq!(model.text(), "", "undoing everything restores the empty start");

                while model.redo() {
                    check_invariants(&model);
                }
                prop_assert_eq!(model.text(), final_text);
            }
        }
    }
}
