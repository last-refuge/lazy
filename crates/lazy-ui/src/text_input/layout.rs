//! Geometry of shaped text: visual rows, caret positions, hit testing, and selection rectangles.
//!
//! Offsets here are byte offsets into the *display* text, which is the model text except in a
//! masked input (one bullet per grapheme) or while the placeholder is shown. The view converts
//! between the two.

use std::ops::Range;

use gpui::{
    Bounds, Font, Hsla, LineLayout, Pixels, Point, SharedString, Size, WrappedLine, point, px, size,
};

use super::unicode::{prev_grapheme_boundary, snap_down};

/// Width added to a selection that spans a line break, so the break is visible.
const NEWLINE_HINT: Pixels = px(6.);

/// One visual row: a logical line, or a soft-wrapped piece of one.
#[derive(Debug, Clone)]
pub(crate) struct Row {
    /// Index of the logical line this row belongs to.
    pub line: usize,
    /// Display offset of the row's first byte.
    pub start: usize,
    /// Display offset just past the row's last byte (for a wrapped row, the next row's start).
    pub end: usize,
    /// Top of the row in content coordinates.
    pub y: Pixels,
    /// Horizontal position of `start` within the unwrapped line.
    pub x_origin: Pixels,
    pub last_in_line: bool,
}

/// Everything that determines how text is shaped. Equal keys give identical layouts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LayoutKey {
    pub revision: u64,
    pub wrap_width: Option<Pixels>,
    pub font: Font,
    pub font_size: Pixels,
    pub line_height: Pixels,
    pub text_color: Hsla,
    pub placeholder_color: Hsla,
    pub placeholder: SharedString,
    pub masked: bool,
}

pub(crate) struct LayoutState {
    pub key: LayoutKey,
    pub lines: Vec<WrappedLine>,
    pub line_starts: Vec<usize>,
    /// Top of each logical line in content coordinates.
    pub line_tops: Vec<Pixels>,
    pub rows: Vec<Row>,
    pub line_height: Pixels,
    /// Size of all content, including rows scrolled out of view.
    pub content_size: Size<Pixels>,
    /// The model revision this layout was shaped for.
    pub revision: u64,
    pub showing_placeholder: bool,
    /// Where the text area was last painted, in window coordinates.
    pub bounds: Option<Bounds<Pixels>>,
}

impl LayoutState {
    pub fn new(
        key: LayoutKey,
        lines: Vec<WrappedLine>,
        line_height: Pixels,
        showing_placeholder: bool,
    ) -> Self {
        let mut line_starts = Vec::with_capacity(lines.len());
        let mut line_tops = Vec::with_capacity(lines.len());
        let mut rows = Vec::new();
        let mut start = 0;
        let mut y = px(0.);
        let mut width = px(0.);

        for (line_ix, line) in lines.iter().enumerate() {
            line_starts.push(start);
            line_tops.push(y);
            let boundaries: Vec<usize> = line
                .wrap_boundaries()
                .iter()
                .map(|boundary| {
                    line.unwrapped_layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix].index
                })
                .collect();
            let mut row_start = 0;
            for (n, row_end) in boundaries.iter().copied().chain([line.len()]).enumerate() {
                rows.push(Row {
                    line: line_ix,
                    start: start + row_start,
                    end: start + row_end,
                    y,
                    x_origin: line.unwrapped_layout.x_for_index(row_start),
                    last_in_line: n == boundaries.len(),
                });
                y += line_height;
                row_start = row_end;
            }
            start += line.len() + 1;
            width = width.max(line.unwrapped_layout.width);
        }

        if rows.is_empty() {
            rows.push(Row {
                line: 0,
                start: 0,
                end: 0,
                y: px(0.),
                x_origin: px(0.),
                last_in_line: true,
            });
            y = line_height;
        }

        let revision = key.revision;
        Self {
            key,
            lines,
            line_starts,
            line_tops,
            rows,
            line_height,
            content_size: size(width, y),
            revision,
            showing_placeholder,
            bounds: None,
        }
    }

    /// The row containing `offset`. An offset at a soft-wrap boundary belongs to the row that
    /// starts there, so the caret appears at the start of the next row.
    pub fn row_for_offset(&self, offset: usize) -> usize {
        self.rows
            .partition_point(|row| row.start <= offset)
            .saturating_sub(1)
    }

    /// Horizontal position of `offset` within row `row_ix`, relative to the row's start.
    pub fn x_in_row(&self, row_ix: usize, offset: usize) -> Pixels {
        let row = &self.rows[row_ix];
        let Some(line) = self.lines.get(row.line) else {
            return px(0.);
        };
        let local = offset.clamp(row.start, row.end) - self.line_starts[row.line];
        line.unwrapped_layout.x_for_index(local) - row.x_origin
    }

    /// Top-left of the caret for `offset`, in content coordinates.
    pub fn point_for_offset(&self, offset: usize) -> Point<Pixels> {
        let row_ix = self.row_for_offset(offset);
        point(self.x_in_row(row_ix, offset), self.rows[row_ix].y)
    }

    /// The last offset of row `row_ix` at which the caret still displays on that row. For a
    /// wrapped row this is before its final grapheme, since the row's end is the next row's start.
    pub fn caret_end_of_row(&self, row_ix: usize) -> usize {
        let row = &self.rows[row_ix];
        match self.lines.get(row.line) {
            Some(line) if !row.last_in_line => {
                let base = self.line_starts[row.line];
                base + prev_grapheme_boundary(&line.text, row.end - base).max(row.start - base)
            }
            _ => row.end,
        }
    }

    /// The display offset nearest to horizontal position `x` within row `row_ix`, never past
    /// [`Self::caret_end_of_row`].
    pub fn offset_in_row(&self, row_ix: usize, x: Pixels) -> usize {
        let row = &self.rows[row_ix];
        let Some(line) = self.lines.get(row.line) else {
            return row.start;
        };
        let base = self.line_starts[row.line];
        let local = nearest_boundary(&line.unwrapped_layout, x + row.x_origin)
            .clamp(row.start - base, row.end - base);
        let local = snap_down(&line.text, local);
        (base + local).min(self.caret_end_of_row(row_ix))
    }

    /// The display offset nearest to `point`, given in content coordinates.
    pub fn offset_for_point(&self, point: Point<Pixels>) -> usize {
        let row_ix = self
            .rows
            .partition_point(|row| row.y + self.line_height <= point.y)
            .min(self.rows.len() - 1);
        self.offset_in_row(row_ix, point.x)
    }

    /// One rectangle per row touched by `range`, in content coordinates.
    pub fn selection_rects(&self, range: Range<usize>) -> Vec<Bounds<Pixels>> {
        let mut rects = Vec::new();
        let first = self.row_for_offset(range.start);
        for row_ix in first..self.rows.len() {
            let row = &self.rows[row_ix];
            if row.start > range.end {
                break;
            }
            let covers_newline = row.last_in_line && range.start <= row.end && range.end > row.end;
            let start = range.start.max(row.start);
            let end = range.end.min(row.end);
            if start > end || (start == end && !covers_newline) {
                continue;
            }
            let x0 = self.x_in_row(row_ix, start);
            let mut x1 = self.x_in_row(row_ix, end);
            if covers_newline {
                x1 += NEWLINE_HINT;
            }
            rects.push(Bounds::new(
                point(x0, row.y),
                size(x1 - x0, self.line_height),
            ));
        }
        rects
    }
}

/// The byte index of the glyph boundary nearest to horizontal position `x`, counting the end of
/// the line as a boundary. (GPUI's own `closest_index_for_x` ignores the final glyph's width, so
/// a click on the left half of the last character would land after it.)
fn nearest_boundary(layout: &LineLayout, x: Pixels) -> usize {
    let mut best = (0, (x - px(0.)).abs());
    let boundaries = layout
        .runs
        .iter()
        .flat_map(|run| {
            run.glyphs
                .iter()
                .map(|glyph| (glyph.index, glyph.position.x))
        })
        .chain([(layout.len, layout.width)]);
    for (index, position) in boundaries {
        let distance = (x - position).abs();
        if distance < best.1 {
            best = (index, distance);
        }
    }
    best.0
}
