//! The workspace rail: a slim column at the window's left edge with one square per workspace
//! and a "+" that adds one (D23).
//!
//! A square shows the workspace's initials; the active one is marked by a bar on its left edge and
//! a ring; the number of things that need the person's attention is a badge (the same number the
//! core's `attention_count` would give if that workspace were active), and unread messages that
//! do not are a dot. The name is the tooltip. A workspace whose connection is down is dimmed.
//!
//! # Keyboard
//!
//! The rail is one tab stop, the first in the window: Up and Down (and Home and End) move
//! between the workspaces and the "+", Enter or Space switches to the workspace under the cursor
//! (or adds one). Ctrl+1 to Ctrl+9 (Cmd on a Mac) switch to the n-th workspace from anywhere in
//! the shell (see [`crate::keyboard`]). After a switch the keyboard goes to the composer of what
//! opened, as it does when a conversation is chosen in the sidebar. There is no animation here to
//! gate on reduced motion.

use gpui::{
    AnyElement, Context, ElementId, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, Role, Styled, Window,
    div, prelude::*, px, transparent_black,
};
use lazy_client_core::{ConnectionState, Input, RailItem};

use crate::app_model::AppModel;
use crate::sidebar::badge_text;
use crate::theme::Theme;

/// The rail's width, wide and in the compact layout.
pub const RAIL_WIDTH: f32 = 64.0;
pub const COMPACT_RAIL_WIDTH: f32 = 48.0;
/// The side of a workspace's square, wide and compact.
const SQUARE: f32 = 40.0;
const COMPACT_SQUARE: f32 = 32.0;

/// What the rail asks the shell to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailEvent {
    /// The "+" was chosen: open "Add a workspace".
    Add,
}

pub struct RailView {
    model: Entity<AppModel>,
    focus: FocusHandle,
    _focus_in: gpui::Subscription,
    /// Where the keyboard is: an index into the workspaces, with the "+" after the last.
    cursor: usize,
}

impl EventEmitter<RailEvent> for RailView {}

impl RailView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe_in(&model, window, |this, _model, _window, cx| {
            let count = this.items(cx).len();
            this.cursor = this.cursor.min(count);
            cx.notify();
        })
        .detach();
        let focus = cx.focus_handle().tab_stop(true);
        // The cursor starts on the current workspace whenever the keyboard arrives.
        let focus_in = cx.on_focus_in(&focus, window, |this, _window, cx| this.on_focus(cx));
        Self {
            model,
            focus,
            _focus_in: focus_in,
            cursor: 0,
        }
    }

    fn items(&self, cx: &gpui::App) -> Vec<RailItem> {
        self.model.read(cx).core().workspace_rail()
    }

    /// The rail's tab stop, for the shell to return to.
    pub fn focus(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// Where the cursor is: the index of a workspace, or the number of workspaces for the "+".
    /// For tests.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether the rail has the keyboard. For tests.
    pub fn has_keyboard(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    fn move_to(&mut self, index: usize, cx: &mut Context<Self>) {
        let last = self.items(cx).len();
        self.cursor = index.min(last);
        cx.notify();
    }

    fn activate(&mut self, index: usize, cx: &mut Context<Self>) {
        let items = self.items(cx);
        match items.get(index) {
            Some(item) => {
                let id = item.id;
                self.cursor = index;
                self.model.update(cx, |model, cx| {
                    model.dispatch(Input::SwitchWorkspace(id), cx)
                });
            }
            None => cx.emit(RailEvent::Add),
        }
        cx.notify();
    }

    /// Puts the cursor on the active workspace (when the keyboard arrives).
    fn on_focus(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.items(cx).iter().position(|item| item.active) {
            self.cursor = active;
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.modified() || !self.focus.is_focused(window) {
            return;
        }
        let last = self.items(cx).len();
        match event.keystroke.key.as_str() {
            "down" => self.move_to((self.cursor + 1).min(last), cx),
            "up" => self.move_to(self.cursor.saturating_sub(1), cx),
            "home" => self.move_to(0, cx),
            "end" => self.move_to(last, cx),
            "enter" | "space" if !event.is_held => self.activate(self.cursor, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    /// What a screen reader and the tooltip say about a workspace.
    pub fn describe(item: &RailItem) -> String {
        let mut text = item.name.clone();
        if item.active {
            text.push_str(", current workspace");
        }
        if item.attention > 0 {
            text.push_str(&format!(
                ", {} need{} your attention",
                item.attention,
                if item.attention == 1 { "s" } else { "" }
            ));
        } else if item.unread {
            text.push_str(", has unread messages");
        }
        match item.connection {
            ConnectionState::Live | ConnectionState::Offline => {}
            ConnectionState::Connecting | ConnectionState::CatchingUp => {
                text.push_str(", connecting");
            }
            ConnectionState::Reconnecting { .. } => text.push_str(", reconnecting"),
        }
        text
    }

    fn square(
        &self,
        theme: &Theme,
        index: usize,
        item: &RailItem,
        size: f32,
        has_keyboard: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let description = Self::describe(item);
        let tip = description.clone();
        let cursor_here = has_keyboard && self.cursor == index;
        let dimmed = !matches!(
            item.connection,
            ConnectionState::Live | ConnectionState::Offline
        );
        div()
            .id(ElementId::NamedInteger("rail-item".into(), index as u64))
            .debug_selector(move || format!("rail-item-{index}"))
            .role(Role::ListItem)
            .aria_label(description)
            .relative()
            .flex_none()
            .w_full()
            .h(px(size + 8.))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .tooltip(move |_, cx| crate::tooltip::TextTooltip::view(tip.clone(), cx))
            .on_click(move |_, _, cx| {
                this.update(cx, |rail, cx| rail.activate(index, cx)).ok();
            })
            // The bar on the left edge of the current workspace.
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(8.))
                    .bottom(px(8.))
                    .w(px(3.))
                    .rounded_r(px(3.))
                    .when(item.active, |bar| bar.bg(colors.rail_text)),
            )
            .child(
                div()
                    .size(px(size))
                    .flex_none()
                    .rounded(theme.radii.large)
                    .border_2()
                    .border_color(if cursor_here {
                        colors.focus_ring
                    } else if item.active {
                        colors.rail_text
                    } else {
                        transparent_black()
                    })
                    .bg(if item.active {
                        colors.accent
                    } else {
                        colors.rail_square
                    })
                    .text_color(if item.active {
                        colors.on_accent
                    } else {
                        colors.rail_text
                    })
                    .when(dimmed, |square| square.opacity(0.6))
                    .flex()
                    .items_center()
                    .justify_center()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(theme.type_scale.body)
                    .child(item.initials.clone()),
            )
            .children((item.attention > 0).then(|| {
                div()
                    .debug_selector(move || format!("rail-badge-{index}"))
                    .absolute()
                    .top(px(-3.))
                    .right(px(5.))
                    .px_1()
                    .rounded(theme.radii.small)
                    .bg(colors.danger)
                    .text_color(colors.on_accent)
                    .text_size(px(10.))
                    .font_weight(FontWeight::BOLD)
                    .child(badge_text(item.attention).unwrap_or_default())
            }))
            .children(
                (item.attention == 0 && item.unread && !item.active).then(|| {
                    div()
                        .debug_selector(move || format!("rail-dot-{index}"))
                        .absolute()
                        .top(px(4.))
                        .right(px(8.))
                        .size(px(9.))
                        .rounded_full()
                        .bg(colors.rail_text)
                        .border_2()
                        .border_color(colors.rail)
                }),
            )
            .into_any_element()
    }
}

impl Focusable for RailView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for RailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        let compact = f32::from(window.viewport_size().width) < crate::shell::COMPACT_BELOW;
        let (width, size) = if compact {
            (COMPACT_RAIL_WIDTH, COMPACT_SQUARE)
        } else {
            (RAIL_WIDTH, SQUARE)
        };
        let items = self.items(cx);
        let has_keyboard = self.focus.is_focused(window);
        let this = cx.entity().downgrade();
        let add_cursor = has_keyboard && self.cursor == items.len();
        let squares: Vec<AnyElement> = items
            .iter()
            .enumerate()
            .map(|(index, item)| self.square(&theme, index, item, size, has_keyboard, cx))
            .collect();
        let add_index = items.len();
        div()
            .id("rail")
            .debug_selector(|| "rail".into())
            .role(Role::List)
            .aria_label("Workspaces")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .w(px(width))
            .flex_none()
            .h_full()
            .bg(colors.rail)
            .flex()
            .flex_col()
            .items_center()
            .pt_2()
            .gap_1()
            .children(squares)
            .child(
                div()
                    .id("rail-add")
                    .debug_selector(|| "rail-add".into())
                    .role(Role::Button)
                    .aria_label("Add a workspace")
                    .tooltip(|_, cx| crate::tooltip::TextTooltip::view("Add a workspace", cx))
                    .flex_none()
                    .w_full()
                    .h(px(size + 8.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        this.update(cx, |rail, cx| rail.activate(add_index, cx))
                            .ok();
                    })
                    .child(
                        div()
                            .size(px(size))
                            .rounded(theme.radii.large)
                            .border_2()
                            // Dashed, so it does not read as a workspace; solid under the cursor.
                            .when(!add_cursor, |square| square.border_dashed())
                            .border_color(if add_cursor {
                                colors.focus_ring
                            } else {
                                colors.rail_text
                            })
                            .text_color(colors.rail_text)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(theme.type_scale.title)
                            .child("+"),
                    ),
            )
    }
}
