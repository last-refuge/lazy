//! The dialogs of the conversation UI. Each is an entity rendering a modal overlay
//! ([`crate::modal`]) and emitting [`crate::modal::DialogEvent::Dismiss`] when it should close.
//! They read the client core through [`crate::app_model::AppModel`] and dispatch its inputs; none
//! performs I/O.

mod add_workspace;
mod browse;
mod create_channel;
mod custom_emoji;
mod delete_message;
mod details;
mod diagnostics;
mod groups;
mod integrations;
mod invitations;
mod people;
mod preferences;
mod profile;
mod quick_switcher;
mod shortcuts;
mod updates;
mod workspace_integrations;
mod workspace_settings;

pub use add_workspace::{AddWorkspaceDialog, Way};
pub use browse::BrowseChannelsDialog;
pub use create_channel::CreateChannelDialog;
pub use custom_emoji::CustomEmojiDialog;
pub use delete_message::DeleteMessageDialog;
pub use details::{DetailsDialog, DetailsRequest};
pub use diagnostics::{
    DesktopInfo, DiagnosticsDialog, report_text, sanitize_server_url, set_desktop_info,
};
pub use groups::GroupsDialog;
pub use integrations::{IntegrationsEvent, IntegrationsSection};
pub use invitations::InvitationsDialog;
pub(crate) use invitations::shown_code;
pub use people::{PeopleDialog, PeopleMode};
pub use preferences::PreferencesDialog;
pub use profile::{ProfileDialog, ProfileRequest};
pub use quick_switcher::{
    QuickSwitcherChoice, QuickSwitcherDialog, QuickSwitcherRun,
    bind_keys as bind_quick_switcher_keys,
};
pub use shortcuts::ShortcutsDialog;
pub use updates::UpdatesDialog;
pub use workspace_integrations::{IntegrationsDialog, IntegrationsView, outcome_words};
pub use workspace_settings::WorkspaceSettingsDialog;

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
