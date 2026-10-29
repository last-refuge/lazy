//! Multiline and single-line text input with full IME support, used by the message composer and
//! the sign-in form.
//!
//! The crate is split so that the rules are testable without a window:
//!
//! * [`model`] and [`unicode`] are pure: text, selection, composition, undo, and UTF-8/UTF-16/
//!   grapheme arithmetic.
//! * [`TextInput`] is the GPUI view: layout, painting, mouse and keyboard handling, and the
//!   platform input handler.
//!
//! # Accessibility
//!
//! The root element exposes a role (`MultilineTextInput`, `TextInput`, or `PasswordInput`), an
//! accessible name, the placeholder, and, except for masked inputs, the full text as its value.
//! Caret and selection positions and per-run text nodes are **not** exposed to assistive
//! technology yet, so a screen reader announces the field and its value but cannot follow the
//! caret. This has not been verified against a real screen reader.

mod element;
mod layout;
pub mod model;
mod style;
pub mod unicode;
mod view;

#[cfg(test)]
mod tests;

pub use model::{Direction, EditorModel, Selection, Unit};
pub use style::{SubmitChord, TextInputOptions, TextInputStyle};
pub use view::{
    KEY_CONTEXT, SuggestionKey, TextInput, TextInputEvent, any_input_composing, bind_keys,
    cancel_any_composition, editing_keystrokes, editing_keystrokes_for, focused_input,
};

/// The GPUI actions the input handles, for hosts that want to bind or dispatch them.
pub mod actions {
    pub use super::view::{
        Backspace, CopySelection, CutSelection, DeleteForward, DeleteWordBackward,
        DeleteWordForward, Enter, FormatBold, FormatItalic, FormatStrike, InsertNewline,
        MoveDocumentEnd, MoveDocumentStart, MoveDown, MoveLeft, MoveLineEnd, MoveLineStart,
        MoveRight, MoveUp, MoveWordLeft, MoveWordRight, PasteClipboard, Redo, SelectAll,
        SelectDocumentEnd, SelectDocumentStart, SelectDown, SelectLeft, SelectLineEnd,
        SelectLineStart, SelectRight, SelectUp, SelectWordLeft, SelectWordRight,
        ShowCharacterPalette, SubmitModified, Tab, TabBackwards, Undo,
    };
}
