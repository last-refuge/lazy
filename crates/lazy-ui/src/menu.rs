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
    /// A keyboard shortcut, shown at the right end of the label.
    pub shortcut: Option<SharedString>,
    pub enabled: bool,
}

impl MenuItem {
    pub fn new(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            detail: None,
            shortcut: None,
            enabled: true,
        }
    }

    pub fn shortcut(mut self, shortcut: impl Into<SharedString>) -> Self {
        self.shortcut = Some(shortcut.into());
        self
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
    /// For a tabbed menu (the command menu): every tab's title and items; `items` is the open
    /// tab's. Left and Right change tab.
    tabs: Vec<(SharedString, Vec<MenuItem>)>,
    tab: usize,
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
            tabs: Vec::new(),
            tab: 0,
            highlighted,
            focus: cx.focus_handle(),
            previous: None,
            label,
        }
    }

    /// A menu of tabs: Left and Right change the tab, Up and Down move within it.
    pub fn tabbed(
        label: &'static str,
        tabs: Vec<(SharedString, Vec<MenuItem>)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let items = tabs.first().map(|tab| tab.1.clone()).unwrap_or_default();
        let mut menu = Self::new(label, items, cx);
        menu.tabs = tabs;
        menu
    }

    /// The items of the open tab (or of the menu), for tests.
    pub fn items(&self) -> &[MenuItem] {
        &self.items
    }

    /// The title of the open tab, for tests.
    pub fn open_tab(&self) -> Option<&str> {
        self.tabs.get(self.tab).map(|tab| tab.0.as_ref())
    }

    /// The highlighted item's id, for tests.
    pub fn highlighted_id(&self) -> Option<&str> {
        self.items
            .get(self.highlighted)
            .map(|item| item.id.as_ref())
    }

    fn switch_tab(&mut self, forwards: bool, cx: &mut Context<Self>) {
        let count = self.tabs.len();
        if count < 2 {
            return;
        }
        self.tab = if forwards {
            (self.tab + 1) % count
        } else {
            (self.tab + count - 1) % count
        };
        self.items = self.tabs[self.tab].1.clone();
        self.highlighted = self.items.iter().position(|item| item.enabled).unwrap_or(0);
        cx.notify();
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
            "left" if !self.tabs.is_empty() => self.switch_tab(false, cx),
            "right" if !self.tabs.is_empty() => self.switch_tab(true, cx),
            "enter" | "space" => self.pick(self.highlighted, window, cx),
            "escape" => self.dismiss(window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }
}

impl Render for Menu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        let label = self.label;
        let mut list = div()
            .id(label)
            .debug_selector(move || label.to_owned())
            .track_focus(&self.focus)
            .occlude()
            // A tabbed menu keeps one width whichever tab is open, so it does not jump.
            .min_w(px(if self.tabs.is_empty() { 200. } else { 340. }))
            // Never taller than the window: a long tab scrolls.
            .max_h(px(
                (f32::from(window.viewport_size().height) - 70.0).max(160.0)
            ))
            .overflow_y_scroll()
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
        if !self.tabs.is_empty() {
            let mut strip = div().flex().gap_1().px_1().pb_1();
            for (index, (title, _)) in self.tabs.iter().enumerate() {
                let open = index == self.tab;
                let name = format!("{label}-tab-{title}");
                strip = strip.child(
                    div()
                        .id(gpui::ElementId::Name(name.clone().into()))
                        .debug_selector(move || name)
                        .px_2()
                        .py_0p5()
                        .rounded(theme.radii.small)
                        .cursor_pointer()
                        .text_size(theme.type_scale.small)
                        .when(open, |tab| {
                            tab.bg(colors.surface_hover)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.text)
                        })
                        .when(!open, |tab| tab.text_color(colors.text_muted))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            while this.tab != index {
                                this.switch_tab(true, cx);
                            }
                        }))
                        .child(title.clone()),
                );
            }
            list = list.child(strip);
        }
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
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_4()
                            .text_size(theme.type_scale.body)
                            .child(crate::emoji_font::label(
                                item.label.clone(),
                                if item.enabled {
                                    colors.text
                                } else {
                                    colors.text_faint
                                },
                                FontWeight::SEMIBOLD,
                            ))
                            .children(item.shortcut.clone().map(|shortcut| {
                                div()
                                    .flex_none()
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.text_faint)
                                    .child(shortcut)
                            })),
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
