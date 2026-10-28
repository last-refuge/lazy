//! A small keyboard-operable popup menu.
//!
//! The menu is an entity that the owner embeds inside a `relative` wrapper around its trigger,
//! positioned with `absolute()` and painted above siblings with `deferred`. It takes focus when
//! opened and gives it back to whatever had it when it closes. Up and Down (or Tab and Shift+Tab)
//! move the highlight, Enter or Space picks, Escape or a click outside dismisses.

use gpui::{
    Context, EventEmitter, FocusHandle, FontWeight, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::*, px,
};

use crate::theme::Theme;

/// One entry in a [`Menu`].
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub id: SharedString,
    pub label: SharedString,
    pub detail: Option<SharedString>,
    pub enabled: bool,
}

impl MenuItem {
    pub fn new(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            detail: None,
            enabled: true,
        }
    }

    pub fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

/// What a menu reports to its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEvent {
    /// The id of the chosen item.
    Select(SharedString),
    Dismiss,
}

pub struct Menu {
    items: Vec<MenuItem>,
    highlighted: usize,
    focus: FocusHandle,
    previous: Option<FocusHandle>,
    /// Prefix of the test labels: `{label}-{item id}`.
    label: &'static str,
}

impl EventEmitter<MenuEvent> for Menu {}

impl Menu {
    pub fn new(label: &'static str, items: Vec<MenuItem>, cx: &mut Context<Self>) -> Self {
        let highlighted = items.iter().position(|item| item.enabled).unwrap_or(0);
        Self {
            items,
            highlighted,
            focus: cx.focus_handle(),
            previous: None,
            label,
        }
    }

    /// Takes focus, remembering where it was.
    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.previous = window.focused(cx);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(previous) = self.previous.take() {
            window.focus(&previous, cx);
        }
    }

    pub fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close(window, cx);
        cx.emit(MenuEvent::Dismiss);
    }

    fn pick(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(index).filter(|item| item.enabled) else {
            return;
        };
        let id = item.id.clone();
        self.close(window, cx);
        cx.emit(MenuEvent::Select(id));
    }

    fn move_highlight(&mut self, forwards: bool, cx: &mut Context<Self>) {
        let count = self.items.len();
        if !self.items.iter().any(|item| item.enabled) {
            return;
        }
        let mut next = self.highlighted;
        for _ in 0..count {
            next = if forwards {
                (next + 1) % count
            } else {
                (next + count - 1) % count
            };
            if self.items[next].enabled {
                break;
            }
        }
        self.highlighted = next;
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.is_held {
            return;
        }
        match event.keystroke.key.as_str() {
            "down" => self.move_highlight(true, cx),
            "up" => self.move_highlight(false, cx),
            "tab" => self.move_highlight(!event.keystroke.modifiers.shift, cx),
            "enter" | "space" => self.pick(self.highlighted, window, cx),
            "escape" => self.dismiss(window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }
}

impl Render for Menu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        let label = self.label;
        let mut list = div()
            .id(label)
            .debug_selector(move || label.to_owned())
            .track_focus(&self.focus)
            .occlude()
            .min_w(px(200.))
            .p_1()
            .flex()
            .flex_col()
            .rounded(theme.radii.medium)
            .bg(colors.surface_raised)
            .border_1()
            .border_color(colors.border_strong)
            .shadow_lg()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key(event, window, cx);
            }))
            .on_mouse_down_out(cx.listener(|this, _, window, cx| this.dismiss(window, cx)));
        for (index, item) in self.items.iter().enumerate() {
            let highlighted = index == self.highlighted && item.enabled;
            let name = format!("{label}-{}", item.id);
            list = list.child(
                div()
                    .id(gpui::ElementId::Name(name.clone().into()))
                    .debug_selector(move || name)
                    .px_3()
                    .py_1p5()
                    .rounded(theme.radii.small)
                    .flex()
                    .flex_col()
                    .when(item.enabled, |row| {
                        row.cursor_pointer()
                            .text_color(colors.text)
                            .when(highlighted, |row| row.bg(colors.surface_hover))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.pick(index, window, cx);
                            }))
                    })
                    .when(!item.enabled, |row| row.text_color(colors.text_faint))
                    .child(
                        div()
                            .text_size(theme.type_scale.body)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(item.label.clone()),
                    )
                    .children(item.detail.clone().map(|detail| {
                        div()
                            .text_size(theme.type_scale.small)
                            .text_color(colors.text_muted)
                            .child(detail)
                    })),
            );
        }
        list
    }
}
