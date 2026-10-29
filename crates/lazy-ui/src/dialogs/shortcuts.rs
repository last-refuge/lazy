//! The keyboard shortcuts dialog (Ctrl+/): every command that has a shortcut, generated from the
//! registry so it cannot drift, plus the keys that belong to the text and the controls.
//!
//! # Keyboard
//!
//! The list is the one tab stop: Up, Down, Page Up, Page Down, Home, and End scroll it. Escape
//! closes, and the keyboard goes back to where it was.

use gpui::{
    Context, EventEmitter, FocusHandle, Focusable, FontWeight, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, ScrollHandle, Styled, Window, div, point, prelude::*, px,
};

use crate::commands;
use crate::modal::{DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, dialog_overlay};
use crate::theme::Theme;

/// Keys that are not commands: the controls' and the text's own.
fn editing_rows() -> Vec<(&'static str, Vec<(&'static str, String)>)> {
    let primary = commands::shortcut_text("secondary-b")
        .trim_end_matches('B')
        .to_owned();
    vec![
        (
            "Moving around",
            vec![
                ("Next or previous control", "Tab, Shift+Tab".to_owned()),
                ("Close a dialog, menu or thread", "Escape".to_owned()),
                ("Move in a list or the rail", "Up, Down".to_owned()),
            ],
        ),
        (
            "Writing",
            vec![
                ("Send the message", "Enter".to_owned()),
                ("New line", "Shift+Enter".to_owned()),
                ("Bold", format!("{primary}B")),
                ("Italic", format!("{primary}I")),
            ],
        ),
    ]
}

pub struct ShortcutsDialog {
    trap: ModalTrap,
    focus: FocusHandle,
    scroll: ScrollHandle,
}

impl EventEmitter<DialogEvent> for ShortcutsDialog {}

impl ShortcutsDialog {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle().tab_stop(true);
        let mut trap = ModalTrap::new(cx);
        trap.open(&focus, window, cx);
        Self {
            trap,
            focus,
            scroll: ScrollHandle::new(),
        }
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }

    /// Scrolls by `by` pixels (positive is down), within the list.
    fn scroll_by(&mut self, by: f32, cx: &mut Context<Self>) {
        let offset = self.scroll.offset();
        let max = self.scroll.max_offset();
        let next = (f32::from(offset.y) - by).clamp(-f32::from(max.y), 0.0);
        self.scroll.set_offset(point(offset.x, px(next)));
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let page = f32::from(self.scroll.bounds().size.height).max(120.0) - 40.0;
        match event.keystroke.key.as_str() {
            "down" => self.scroll_by(40.0, cx),
            "up" => self.scroll_by(-40.0, cx),
            "pagedown" => self.scroll_by(page, cx),
            "pageup" => self.scroll_by(-page, cx),
            "home" => self.scroll_by(-1.0e6, cx),
            "end" => self.scroll_by(1.0e6, cx),
            _ => return,
        }
        cx.stop_propagation();
    }
}

impl Focusable for ShortcutsDialog {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ShortcutsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        let close_to = cx.entity().downgrade();
        let heading = |text: &'static str| {
            div()
                .pt_2()
                .text_size(theme.type_scale.small)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.text_muted)
                .child(text)
        };
        let row = |title: String, keys: String| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_4()
                .py_0p5()
                .child(
                    div()
                        .text_size(theme.type_scale.body)
                        .text_color(colors.text)
                        .child(title),
                )
                .child(
                    div()
                        .flex_none()
                        .px_2()
                        .py_0p5()
                        .rounded(theme.radii.small)
                        .bg(colors.surface_hover)
                        .border_1()
                        .border_color(colors.border)
                        .text_size(theme.type_scale.small)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.text)
                        .child(keys),
                )
        };
        let mut list = div()
            .id("shortcuts-list")
            .debug_selector(|| "shortcuts-list".into())
            .track_focus(&self.focus)
            .track_scroll(&self.scroll)
            .overflow_y_scroll()
            .max_h(px(
                (f32::from(window.viewport_size().height) - 220.0).clamp(160.0, 720.0)
            ))
            .focus_visible(move |style| style.border_color(colors.focus_ring))
            .border_1()
            .border_color(gpui::transparent_black())
            .rounded(theme.radii.medium)
            .px_2()
            .flex()
            .flex_col()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| this.on_key(event, cx)));
        for (group, rows) in commands::shortcut_rows() {
            list = list.child(heading(group.title()));
            for (title, keys) in rows {
                list = list.child(row(title, keys));
            }
        }
        for (group, rows) in editing_rows() {
            list = list.child(heading(group));
            for (title, keys) in rows {
                list = list.child(row(title.to_owned(), keys));
            }
        }
        dialog_overlay(
            &theme,
            "Keyboard shortcuts",
            560.,
            &self.trap,
            list.into_any_element(),
            "Close",
            move |window, cx| {
                close_to
                    .update(cx, |dialog, cx| dialog.dismiss(window, cx))
                    .ok();
            },
        )
        .on_action(cx.listener(|this, _: &DialogDismiss, window, cx| this.dismiss(window, cx)))
        .on_action(cx.listener(|this, _: &DialogNext, window, cx| this.trap.tab(false, window, cx)))
        .on_action(cx.listener(|this, _: &DialogPrev, window, cx| this.trap.tab(true, window, cx)))
    }
}
