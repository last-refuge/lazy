//! Visual and behavioral configuration for a [`TextInput`](super::TextInput).

use gpui::{Hsla, Pixels, SharedString, px, rgb, rgba};

use super::model::DEFAULT_MAX_BYTES;
use crate::theme::Theme;

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

impl TextInputStyle {
    /// The style that takes its colors and metrics from `theme`. Text inputs derive this from the
    /// live theme on every render, so they follow a change of system appearance.
    pub fn from_theme(theme: &Theme) -> Self {
        Self {
            text_color: theme.colors.text,
            placeholder_color: theme.colors.text_faint,
            selection_color: theme.colors.selection,
            caret_color: theme.colors.caret,
            font_size: theme.type_scale.body,
            line_height: theme.type_scale.line_height_body,
            padding_x: px(0.),
            padding_y: px(0.),
            caret_width: px(1.5),
        }
    }
}

impl Default for TextInputStyle {
    fn default() -> Self {
        Self {
            // The brand's light palette (Forest text, muted placeholder, Sage selection), for an
            // input that is not themed.
            text_color: rgb(0x183f35).into(),
            placeholder_color: rgb(0x50655a).into(),
            selection_color: rgba(0xa9c3a0ff).into(),
            caret_color: rgb(0x183f35).into(),
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
    /// Follow the live theme (the default): the style is re-derived from the theme on every render
    /// when one is installed. [`TextInputOptions::style`] turns this off for a fixed style.
    pub themed: bool,
    pub enabled: bool,
    /// For single-line inputs embedded in a list-like control: Up and Down, and Backspace in an
    /// empty input, are not consumed but propagate to the parent, which can use them to move a
    /// highlight or remove a chip. Off by default, where Up and Down move the caret to the start
    /// or end.
    pub pass_through_navigation: bool,
    /// Ctrl/Cmd+B, Ctrl/Cmd+I, and Ctrl/Cmd+Shift+X wrap the selection in the message syntax's
    /// bold, italic, and strikethrough markers. Multiline inputs only. Off by default, so that
    /// plain fields (names, topics, passwords) do not claim the keys.
    pub formatting_shortcuts: bool,
    /// Pressing Up in an empty multiline input emits [`TextInputEvent::UpWhenEmpty`] instead of
    /// doing nothing, so a composer can offer to edit the previous message.
    ///
    /// [`TextInputEvent::UpWhenEmpty`]: super::TextInputEvent::UpWhenEmpty
    pub report_up_when_empty: bool,
    /// Pasting files or an image (and no text) emits [`TextInputEvent::PasteAttachments`] instead
    /// of inserting anything, so a composer can attach them. Pasting text is unchanged.
    ///
    /// [`TextInputEvent::PasteAttachments`]: super::TextInputEvent::PasteAttachments
    pub paste_attachments: bool,
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
            themed: true,
            enabled: true,
            pass_through_navigation: false,
            formatting_shortcuts: false,
            report_up_when_empty: false,
            paste_attachments: false,
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

    /// Uses a fixed style, which no longer follows the theme.
    pub fn style(mut self, style: TextInputStyle) -> Self {
        self.style = style;
        self.themed = false;
        self
    }

    pub fn themed(mut self, themed: bool) -> Self {
        self.themed = themed;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn pass_through_navigation(mut self, pass_through: bool) -> Self {
        self.pass_through_navigation = pass_through;
        self
    }

    pub fn formatting_shortcuts(mut self, enabled: bool) -> Self {
        self.formatting_shortcuts = enabled;
        self
    }

    pub fn report_up_when_empty(mut self, enabled: bool) -> Self {
        self.report_up_when_empty = enabled;
        self
    }

    pub fn paste_attachments(mut self, enabled: bool) -> Self {
        self.paste_attachments = enabled;
        self
    }
}
