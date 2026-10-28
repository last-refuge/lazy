//! The dialogs of the conversation UI. Each is an entity rendering a modal overlay
//! ([`crate::modal`]) and emitting [`crate::modal::DialogEvent::Dismiss`] when it should close.
//! They read the client core through [`crate::app_model::AppModel`] and dispatch its inputs; none
//! performs I/O.

mod browse;
mod create_channel;
mod details;
mod people;

pub use browse::BrowseChannelsDialog;
pub use create_channel::CreateChannelDialog;
pub use details::{DetailsDialog, DetailsRequest};
pub use people::{PeopleDialog, PeopleMode};

use gpui::{App, AppContext as _, Context, Entity};

use crate::text_input::{SubmitChord, TextInput, TextInputOptions};

/// A single-line text field styled with the theme, for dialogs.
pub(crate) fn single_line_field<T: 'static>(
    placeholder: &'static str,
    label: &'static str,
    max_bytes: usize,
    cx: &mut Context<T>,
) -> Entity<TextInput> {
    cx.new(|cx| {
        TextInput::new(
            TextInputOptions::new(placeholder)
                .accessible_label(label)
                .single_line(true)
                .max_bytes(max_bytes),
            cx,
        )
    })
}

/// Whether any of the inputs is mid-composition, which makes Escape theirs.
pub(crate) fn any_composing(inputs: &[&Entity<TextInput>], cx: &App) -> bool {
    inputs.iter().any(|input| input.read(cx).is_composing())
}

/// A multi-line field where Enter adds a line and Ctrl+Enter submits, for descriptions.
pub(crate) fn multiline_field<T: 'static>(
    placeholder: &'static str,
    label: &'static str,
    max_bytes: usize,
    cx: &mut Context<T>,
) -> Entity<TextInput> {
    cx.new(|cx| {
        TextInput::new(
            TextInputOptions::new(placeholder)
                .accessible_label(label)
                .max_lines(4)
                .max_bytes(max_bytes)
                .submit_chord(SubmitChord::ModifierEnter),
            cx,
        )
    })
}
