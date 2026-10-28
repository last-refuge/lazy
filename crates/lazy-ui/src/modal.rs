//! Modal dialogs: a backdrop that blocks the window behind it, focus that stays inside, Escape to
//! dismiss, and focus restored to where it was when the dialog closes.
//!
//! A dialog is an entity that renders [`dialog_overlay`] and owns a [`ModalTrap`]. The host (the
//! shell) shows it above everything else and drops it when the dialog emits
//! [`DialogEvent::Dismiss`].
//!
//! # How the focus trap works
//!
//! GPUI's Tab order covers the whole window, so a dialog fences its controls between two
//! zero-size tab stops (sentinels). When Tab or Shift+Tab would land on a sentinel, or leave the
//! dialog altogether, focus wraps to the far end instead. The dialog's own key context
//! (`LazyDialog`) takes Tab, Shift+Tab, and Escape before the shell's bindings can.
//!
//! # Text input
//!
//! Escape while an input method is composing belongs to the composition (it cancels the
//! pre-edit text), so a dialog must not dismiss then: check [`ModalTrap::dismiss_allowed`].

use gpui::{
    AnyElement, App, Div, FocusHandle, FontWeight, InteractiveElement, KeyBinding, MouseButton,
    ParentElement, SharedString, Styled, Window, actions, div, prelude::*, px,
};

use crate::theme::Theme;

/// Key context of an open dialog.
pub const DIALOG_CONTEXT: &str = "LazyDialog";

actions!(lazy_dialog, [DialogDismiss, DialogNext, DialogPrev]);

/// Emitted by a dialog entity when it should be closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogEvent {
    Dismiss,
}

/// Registers the dialog key bindings. Called once from [`crate::init`].
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", DialogDismiss, Some(DIALOG_CONTEXT)),
        KeyBinding::new("tab", DialogNext, Some(DIALOG_CONTEXT)),
        KeyBinding::new("shift-tab", DialogPrev, Some(DIALOG_CONTEXT)),
    ]);
}

/// Focus bookkeeping for one open dialog.
pub struct ModalTrap {
    /// Wraps the dialog's content; focus inside it counts as "in the dialog".
    container: FocusHandle,
    start: FocusHandle,
    end: FocusHandle,
    /// What had focus before the dialog opened.
    previous: Option<FocusHandle>,
}

impl ModalTrap {
    pub fn new(cx: &mut App) -> Self {
        Self {
            container: cx.focus_handle(),
            start: cx.focus_handle().tab_stop(true),
            end: cx.focus_handle().tab_stop(true),
            previous: None,
        }
    }

    /// Remembers the current focus and moves it to `initial`.
    pub fn open(&mut self, initial: &FocusHandle, window: &mut Window, cx: &mut App) {
        self.previous = window.focused(cx);
        window.focus(initial, cx);
    }

    /// Puts focus back where it was before [`open`](Self::open).
    pub fn close(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(previous) = self.previous.take() {
            window.focus(&previous, cx);
        }
    }

    /// Whether Escape may dismiss the dialog. False while any input method is composing.
    pub fn dismiss_allowed(composing: bool) -> bool {
        !composing
    }

    /// Moves focus to the next (or previous) control inside the dialog, wrapping at the ends.
    pub fn tab(&self, backwards: bool, window: &mut Window, cx: &mut App) {
        if backwards {
            window.focus_prev(cx);
        } else {
            window.focus_next(cx);
        }
        let escaped = !self.container.contains_focused(window, cx);
        if backwards {
            if escaped || self.start.is_focused(window) {
                window.focus(&self.end, cx);
                window.focus_prev(cx);
            }
        } else if escaped || self.end.is_focused(window) {
            window.focus(&self.start, cx);
            window.focus_next(cx);
        }
    }

    fn sentinel(handle: &FocusHandle) -> Div {
        div().size_0().track_focus(handle)
    }
}

/// The full-window overlay: a scrim that blocks pointer input to everything behind it, and a
/// centered card with a title, a close button, and `body`.
///
/// Attach the `DialogDismiss`, `DialogNext`, and `DialogPrev` action handlers to the returned
/// element. `on_scrim_click` runs when the backdrop (not the card) is clicked.
pub fn dialog_overlay(
    theme: &Theme,
    title: impl Into<SharedString>,
    width: f32,
    trap: &ModalTrap,
    body: AnyElement,
    close_label: &'static str,
    on_close: impl Fn(&mut Window, &mut App) + 'static,
) -> Div {
    let colors = theme.colors;
    let on_close = std::rc::Rc::new(on_close);
    let on_scrim = on_close.clone();
    let on_x = on_close;
    div()
        .absolute()
        .inset_0()
        .occlude()
        .debug_selector(|| "dialog-scrim".into())
        .bg(colors.scrim)
        .flex()
        .items_center()
        .justify_center()
        .key_context(DIALOG_CONTEXT)
        .on_mouse_down(MouseButton::Left, move |_, window, cx| on_scrim(window, cx))
        .child(
            div()
                .id("dialog-card")
                .debug_selector(|| "dialog-card".into())
                .track_focus(&trap.container)
                .occlude()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .w(px(width))
                .max_w_full()
                .max_h_full()
                .flex()
                .flex_col()
                .rounded(theme.radii.large)
                .bg(colors.surface_raised)
                .border_1()
                .border_color(colors.border_strong)
                .shadow_lg()
                .child(ModalTrap::sentinel(&trap.start))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_5()
                        .pt_4()
                        .pb_2()
                        .child(
                            div()
                                .text_size(theme.type_scale.title)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.text)
                                .child(title.into()),
                        )
                        .child(
                            div()
                                .id("dialog-close")
                                .debug_selector(|| "dialog-close".into())
                                .cursor_pointer()
                                .px_2()
                                .rounded(theme.radii.small)
                                .text_size(theme.type_scale.body)
                                .text_color(colors.text_muted)
                                .hover(move |style| style.bg(colors.surface_hover))
                                .on_click(move |_, window, cx| on_x(window, cx))
                                .child(close_label),
                        ),
                )
                .child(div().px_5().pb_5().flex().flex_col().gap_3().child(body))
                .child(ModalTrap::sentinel(&trap.end)),
        )
}
