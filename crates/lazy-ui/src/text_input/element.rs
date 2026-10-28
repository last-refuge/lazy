//! The custom GPUI element that shapes, lays out, and paints the text.
//!
//! Layout runs inside GPUI's measure callback because the number of wrapped rows, and so the
//! input's height, depends on the width the parent grants. Shaping is cached by [`LayoutKey`], so
//! frames that change nothing do no shaping work.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AvailableSpace, Bounds, ContentMask, Element, ElementId, ElementInputHandler, Entity,
    GlobalElementId, InspectorElementId, IntoElement, LayoutId, Pixels, Size, Style, TextAlign,
    TextRun, TextStyle, UnderlineStyle, Window, fill, point, px, relative,
};

use super::layout::{LayoutKey, LayoutState};
use super::view::TextInput;

pub(crate) struct TextElement {
    pub input: Entity<TextInput>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let input = self.input.clone();
        let cell = input.read(cx).layout.clone();
        let text_style = window.text_style();
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let layout_id =
            window.request_measured_layout(style, move |known, available, window, cx| {
                measure(&input, &cell, &text_style, known, available, window, cx)
            });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> Self::PrepaintState {
        let notify = self.input.update(cx, |input, _| {
            input.resolve_scroll(bounds);
            std::mem::take(&mut input.platform_dirty)
        });
        if notify {
            let handle = self.input.read(cx).focus_handle.clone();
            window.notify_text_input_changed(&handle);
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        let input = self.input.read(cx);
        let focus_handle = input.focus_handle.clone();
        let cell = input.layout.clone();
        let style = input.style.clone();
        let scroll = input.scroll;
        let revision = input.model.revision();
        let editable = input.is_enabled();
        let selection = input.model.selection();
        let selection_display = input.display_offset(selection.range().start)
            ..input.display_offset(selection.range().end);
        let caret_display = input.display_offset(selection.head);

        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );

        let mut layout = cell.borrow_mut();
        let Some(layout) = layout.as_mut() else {
            return;
        };
        layout.bounds = Some(bounds);
        if layout.revision != revision {
            // The text changed after this layout was shaped; the next frame lays it out again.
            return;
        }

        let focused = focus_handle.is_focused(window);
        let origin = bounds.origin - scroll;
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            if focused && !selection.is_empty() {
                for rect in layout.selection_rects(selection_display) {
                    let rect = Bounds::new(origin + rect.origin, rect.size);
                    window.paint_quad(fill(rect, style.selection_color));
                }
            }
            for (line, top) in layout.lines.iter().zip(&layout.line_tops) {
                let at = origin + point(px(0.), *top);
                if let Err(error) =
                    line.paint(at, layout.line_height, TextAlign::Left, None, window, cx)
                {
                    tracing::warn!(%error, "failed to paint a line of text input");
                }
            }
            if focused && editable && selection.is_empty() {
                let caret = layout.point_for_offset(caret_display);
                let rect = Bounds::new(
                    origin + caret,
                    gpui::size(style.caret_width, layout.line_height),
                );
                window.paint_quad(fill(rect, style.caret_color));
            }
        });
    }
}

/// GPUI's measure callback: shapes the text for the granted width (if the cached layout does not
/// already match) and reports the size of the visible area.
fn measure(
    input: &Entity<TextInput>,
    cell: &Rc<RefCell<Option<LayoutState>>>,
    text_style: &TextStyle,
    known: Size<Option<Pixels>>,
    available: Size<AvailableSpace>,
    window: &mut Window,
    cx: &mut gpui::App,
) -> Size<Pixels> {
    let this = input.read(cx);
    let wrap_width = if this.single_line {
        None
    } else {
        known.width.or(match available.width {
            AvailableSpace::Definite(width) => Some(width),
            _ => None,
        })
    };
    let font = text_style.font();
    let key = LayoutKey {
        revision: this.model.revision(),
        wrap_width,
        font: font.clone(),
        font_size: this.style.font_size,
        line_height: this.style.line_height,
        text_color: this.style.text_color,
        placeholder_color: this.style.placeholder_color,
        placeholder: this.placeholder.clone(),
        masked: this.masked,
    };

    let up_to_date = cell.borrow().as_ref().is_some_and(|state| state.key == key);
    if !up_to_date {
        let showing_placeholder = this.model.is_empty();
        let text = this.display_text();
        let runs = runs_for(this, text.len(), &font, showing_placeholder);
        let lines = match window.text_system().shape_text(
            text,
            this.style.font_size,
            &runs,
            wrap_width,
            None,
        ) {
            Ok(lines) => lines.into_iter().collect(),
            Err(error) => {
                tracing::warn!(%error, "failed to shape text input content");
                Vec::new()
            }
        };
        let state = LayoutState::new(key, lines, this.style.line_height, showing_placeholder);
        *cell.borrow_mut() = Some(state);
    }

    let guard = cell.borrow();
    let Some(state) = guard.as_ref() else {
        return Size::default();
    };
    let rows = if this.single_line || state.showing_placeholder {
        1
    } else {
        state.rows.len().clamp(1, this.max_lines)
    };
    Size {
        width: known.width.unwrap_or(state.content_size.width),
        height: state.line_height * rows as f32,
    }
}

/// Text runs covering the whole display text, underlining the IME preedit range.
fn runs_for(
    this: &TextInput,
    display_len: usize,
    font: &gpui::Font,
    showing_placeholder: bool,
) -> Vec<TextRun> {
    let color = if showing_placeholder {
        this.style.placeholder_color
    } else {
        this.style.text_color
    };
    let run = |len: usize, underline: Option<UnderlineStyle>| TextRun {
        len,
        font: font.clone(),
        color,
        background_color: None,
        underline,
        strikethrough: None,
    };
    let marked = if showing_placeholder {
        None
    } else {
        this.model.marked_range()
    };
    let Some(marked) = marked else {
        return vec![run(display_len, None)];
    };
    let start = this.display_offset(marked.start).min(display_len);
    let end = this.display_offset(marked.end).clamp(start, display_len);
    let underline = UnderlineStyle {
        color: Some(color),
        thickness: px(1.),
        wavy: false,
    };
    [
        run(start, None),
        run(end - start, Some(underline)),
        run(display_len - end, None),
    ]
    .into_iter()
    .filter(|run| run.len > 0)
    .collect()
}
