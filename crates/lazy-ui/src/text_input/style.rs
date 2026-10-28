//! Visual and behavioral configuration for a [`TextInput`](super::TextInput).

use gpui::{Hsla, Pixels, SharedString, px, rgb, rgba};

use super::model::DEFAULT_MAX_BYTES;

/// Colors and metrics. The input draws nothing else: backgrounds, borders, and focus rings belong
/// to the host, which can feed its theme tokens in here.
#[derive(Debug, Clone, PartialEq)]
pub struct TextInputStyle {
    pub text_color: Hsla,
    pub placeholder_color: Hsla,
    pub selection_color: Hsla,
    pub caret_color: Hsla,
    pub font_size: Pixels,
    /// Height of one text row. Every row, wrapped or not, has this height.
    pub line_height: Pixels,
    pub padding_x: Pixels,
    pub padding_y: Pixels,
    pub caret_width: Pixels,
}

impl Default for TextInputStyle {
    fn default() -> Self {
        Self {
            text_color: rgb(0x2b2622).into(),
            placeholder_color: rgb(0x6f665d).into(),
            selection_color: rgba(0x2f6fed40).into(),
            caret_color: rgb(0x2b2622).into(),
            font_size: px(15.),
            line_height: px(22.),
            padding_x: px(0.),
            padding_y: px(0.),
            caret_width: px(1.5),
        }
    }
}

/// Which key submits a multiline input. Single-line inputs always submit on Enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubmitChord {
    /// Enter submits and Shift+Enter inserts a line break.
    #[default]
    Enter,
    /// Ctrl/Cmd+Enter submits and Enter inserts a line break.
    ModifierEnter,
}

/// Construction options. Fields are public; the builder methods are shorthand.
#[derive(Debug, Clone)]
pub struct TextInputOptions {
    pub placeholder: SharedString,
    /// Name announced by assistive technology.
    pub accessible_label: SharedString,
    /// One row, no wrapping, horizontal scrolling. Line breaks in pasted text become spaces,
    /// Enter always submits, and Tab/Shift+Tab are reported as [`TextInputEvent::Tab`] so the
    /// host can move focus.
    ///
    /// [`TextInputEvent::Tab`]: super::TextInputEvent::Tab
    pub single_line: bool,
    /// Shows one bullet per grapheme instead of the text. Copy and cut are disabled, word
    /// navigation moves across the whole field, and the value is withheld from assistive
    /// technology. `TextInput::text` still returns the real value.
    pub masked: bool,
    /// Rows shown before a multiline input starts scrolling internally.
    pub max_lines: usize,
    /// Capacity in UTF-8 bytes; further input is truncated on a grapheme boundary.
    pub max_bytes: usize,
    pub submit_chord: SubmitChord,
    pub style: TextInputStyle,
    pub enabled: bool,
}

impl TextInputOptions {
    pub fn new(placeholder: impl Into<SharedString>) -> Self {
        Self {
            placeholder: placeholder.into(),
            accessible_label: SharedString::default(),
            single_line: false,
            masked: false,
            max_lines: 8,
            max_bytes: DEFAULT_MAX_BYTES,
            submit_chord: SubmitChord::Enter,
            style: TextInputStyle::default(),
            enabled: true,
        }
    }

    pub fn accessible_label(mut self, label: impl Into<SharedString>) -> Self {
        self.accessible_label = label.into();
        self
    }

    pub fn single_line(mut self, single_line: bool) -> Self {
        self.single_line = single_line;
        self
    }

    pub fn masked(mut self, masked: bool) -> Self {
        self.masked = masked;
        self
    }

    pub fn max_lines(mut self, max_lines: usize) -> Self {
        self.max_lines = max_lines.max(1);
        self
    }

    pub fn max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    pub fn submit_chord(mut self, chord: SubmitChord) -> Self {
        self.submit_chord = chord;
        self
    }

    pub fn style(mut self, style: TextInputStyle) -> Self {
        self.style = style;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}
