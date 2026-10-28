//! Keyboard navigation shared by the shell's controls.
//!
//! Tab and Shift+Tab move between focusable controls (sidebar channels, the composer, Send,
//! Sign out, a failed message's retry button). Enter or Space activates a focused control. Text
//! inputs keep Tab for themselves only where they are single-line (the sign-in form moves focus
//! itself); in the multiline composer a bare Tab is not an edit, so it navigates.

use gpui::{App, KeyBinding, KeyDownEvent, actions};

/// Key context of the signed-in shell; the bindings below apply inside it.
pub const SHELL_CONTEXT: &str = "LazyShell";

actions!(lazy_ui, [FocusNextControl, FocusPrevControl]);

/// Registers the navigation key bindings. Called once from [`crate::init`].
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", FocusNextControl, Some(SHELL_CONTEXT)),
        KeyBinding::new("shift-tab", FocusPrevControl, Some(SHELL_CONTEXT)),
    ]);
}

/// Whether a key press should activate the focused control.
pub fn is_activation(event: &KeyDownEvent) -> bool {
    !event.is_held
        && event.keystroke.modifiers == gpui::Modifiers::default()
        && matches!(event.keystroke.key.as_str(), "enter" | "space")
}
