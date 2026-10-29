//! Saved messages: what the person kept with "Save for later", most recently saved first, in
//! place of the conversation.
//!
//! Each row says where the message is, who wrote it and when, what it said, and when it was
//! saved. Enter or a click jumps to the message in its conversation (a reply opens in its
//! thread); Remove, or the Delete key, takes it out of the list.
//!
//! # Keyboard
//!
//! The list is one tab stop and starts with the keyboard: Up and Down move the active row, Page
//! Up and Page Down by a screenful, Home and End to the first and last, Enter opens the active
//! message, Delete removes it from the saved list, Escape closes the view. Moving to the last
//! row asks for the next page. Tab order: the list, "Show more" or "Try again", and Close.

use gpui::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    KeyDownEvent, ParentElement, Render, Role, ScrollStrategy, Styled, Subscription,
    UniformListScrollHandle, Window, div, prelude::*, px, transparent_black, uniform_list,
};
use jiff::Zoned;
use jiff::tz::TimeZone;
use lazy_client_core::{Change, ClientCore, Input, LoadStatus};
use lazy_domain::{ConversationId, ConversationKind, MessageId, Timestamp};
use lazy_protocol::{Conversation, SavedItem};

use crate::app_model::{AppEvent, AppModel};
use crate::form::{self, ButtonKind};
use crate::inbox_view::{excerpt, note_block};
use crate::states;
use crate::theme::Theme;
use crate::time_fmt::{ago_label, to_zoned};

/// The height of a row, so the list never re-measures.
pub const ROW_HEIGHT: f32 = 112.0;
const SKELETON_ROWS: usize = 6;
const FALLBACK_PAGE: usize = 4;

/// What the saved view asks the shell to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavedEvent {
    /// Escape or Close: leave the view and give the keyboard back.
    Close,
    /// Enter or a click on a row: go to the message in its conversation.
    Open {
        conversation: ConversationId,
        message: MessageId,
    },
}

/// One saved message, ready to draw.
#[derive(Debug, Clone)]
struct Row {
    conversation: ConversationId,
    message: MessageId,
    place: String,
    author: String,
    written: Timestamp,
    saved: Timestamp,
    snippet: String,
    reply: bool,
    /// The person is not in the conversation any more (opening it offers to join).
    outside: bool,
}

/// What a row says, for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowInfo {
    pub place: String,
    pub author: String,
    pub snippet: String,
    pub reply: bool,
    pub message: MessageId,
}

/// Where a conversation is, in words: `#design`, or the other people's names.
pub(crate) fn place_label(core: &ClientCore, conversation: &Conversation) -> String {
    match conversation.kind {
        ConversationKind::Channel => format!(
            "#{}",
            conversation
                .name
                .as_ref()
                .map_or_else(|| "channel".to_owned(), ToString::to_string)
        ),
        ConversationKind::Direct | ConversationKind::Group => {
            let me = core.me().map(|user| user.id);
            let mut names: Vec<&str> = conversation
                .participants
                .iter()
                .filter(|user| Some(**user) != me)
                .map(|user| core.display_name(*user))
                .collect();
            names.sort_by_key(|name| name.to_lowercase());
            if names.is_empty() {
                "Direct message".to_owned()
            } else {
                names.join(", ")
            }
        }
    }
}

pub struct SavedMessagesView {
    model: Entity<AppModel>,
    scroll: UniformListScrollHandle,
    active: Option<usize>,
    rows: Vec<Row>,
    zone: TimeZone,
    /// A message is being found: the view says so until the shell is done.
    opening: bool,
    list_focus: FocusHandle,
    more_focus: FocusHandle,
    retry_focus: FocusHandle,
    close_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SavedEvent> for SavedMessagesView {}

impl SavedMessagesView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe_in(
            &model,
            window,
            |this, _model, event: &AppEvent, _window, cx| {
                let AppEvent::Changed(changes) = event;
                if changes
                    .iter()
                    .any(|change| matches!(change, Change::SavedChanged))
                {
                    this.sync_rows(cx);
                }
                cx.notify();
            },
        );
        let mut view = Self {
            model,
            scroll: UniformListScrollHandle::new(),
            active: None,
            rows: Vec::new(),
            zone: TimeZone::system(),
            opening: false,
            list_focus: cx.focus_handle().tab_stop(true),
            more_focus: cx.focus_handle().tab_stop(true),
            retry_focus: cx.focus_handle().tab_stop(true),
            close_focus: cx.focus_handle().tab_stop(true),
            _subscriptions: vec![subscription],
        };
        view.sync_rows(cx);
        view
    }

    // ----- For the shell and tests -----

    /// What each row says. For tests.
    pub fn rows_info(&self) -> Vec<RowInfo> {
        self.rows
            .iter()
            .map(|row| RowInfo {
                place: row.place.clone(),
                author: row.author.clone(),
                snippet: row.snippet.clone(),
                reply: row.reply,
                message: row.message,
            })
            .collect()
    }

    pub fn active(&self) -> Option<usize> {
        self.active
    }

    /// Which tab stop has the keyboard. For tests.
    pub fn stop(&self, window: &Window) -> &'static str {
        if self.list_focus.is_focused(window) {
            "list"
        } else if self.more_focus.is_focused(window) {
            "more"
        } else if self.retry_focus.is_focused(window) {
            "retry"
        } else if self.close_focus.is_focused(window) {
            "close"
        } else {
            "elsewhere"
        }
    }

    /// Starts afresh with the keyboard in the list.
    pub fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.active = None;
        self.rows.clear();
        self.opening = false;
        window.focus(&self.list_focus, cx);
        self.sync_rows(cx);
        cx.notify();
    }

    /// The shell is finding the chosen message: say so while it does.
    pub fn set_opening(&mut self, opening: bool, cx: &mut Context<Self>) {
        self.opening = opening;
        cx.notify();
    }

    // ----- Reading the core -----

    fn dispatch(&self, input: Input, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
    }

    fn sync_rows(&mut self, cx: &mut Context<Self>) {
        let core = self.model.read(cx).core();
        let view = core.saved();
        self.rows = view.items.iter().map(|item| row_of(core, item)).collect();
        self.active = self.active.filter(|at| *at < self.rows.len());
    }

    // ----- Moving and acting -----

    fn page(&self) -> usize {
        let state = self.scroll.0.borrow();
        state
            .last_item_size
            .map(|size| (f32::from(size.item.height) / ROW_HEIGHT).floor() as usize)
            .filter(|rows| *rows > 0)
            .unwrap_or(FALLBACK_PAGE)
    }

    fn move_to(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        let index = index.min(self.rows.len() - 1);
        self.active = Some(index);
        self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
        if index + 1 == self.rows.len() {
            self.load_more(cx);
        }
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let next = match self.active {
            None if delta > 0 => 0,
            None => return,
            Some(at) => (at as isize + delta).clamp(0, self.rows.len() as isize - 1) as usize,
        };
        self.move_to(next, cx);
    }

    fn load_more(&self, cx: &mut Context<Self>) {
        let more = {
            let view = self.model.read(cx).core().saved();
            view.has_more && matches!(view.status, LoadStatus::Ready)
        };
        if more {
            self.dispatch(Input::LoadMoreSaved, cx);
        }
    }

    fn open(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get(index) {
            cx.emit(SavedEvent::Open {
                conversation: row.conversation,
                message: row.message,
            });
        }
    }

    fn remove(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(index) else {
            return;
        };
        let message_id = row.message;
        self.dispatch(
            Input::SetMessageSaved {
                message_id,
                saved: false,
            },
            cx,
        );
        // The core drops the row at once: keep the choice where it was, within the list.
        self.sync_rows(cx);
        self.active = match self.rows.len() {
            0 => None,
            len => Some(index.min(len - 1)),
        };
        cx.notify();
    }

    /// Asks again: for the next page when there are rows, else for the first (opening a list
    /// that failed to load asks again). The keyboard moves to the list, which is always there:
    /// the button it was on goes away.
    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let has_rows = !self.rows.is_empty();
        if has_rows {
            self.dispatch(Input::LoadMoreSaved, cx);
        } else {
            self.dispatch(Input::OpenSaved, cx);
        }
        window.focus(&self.list_focus, cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let modifiers = event.keystroke.modifiers;
        if modifiers.modified() {
            return;
        }
        match event.keystroke.key.as_str() {
            "down" => self.step(1, cx),
            "up" => self.step(-1, cx),
            "pagedown" => self.step(self.page() as isize, cx),
            "pageup" => self.step(-(self.page() as isize), cx),
            "home" => self.move_to(0, cx),
            "end" => self.move_to(self.rows.len().saturating_sub(1), cx),
            "enter" if self.list_focus.is_focused(window) && !event.is_held => {
                if let Some(index) = self.active {
                    self.open(index, cx);
                }
            }
            "delete" | "backspace" if self.list_focus.is_focused(window) && !event.is_held => {
                if let Some(index) = self.active {
                    self.remove(index, cx);
                }
            }
            "escape" => cx.emit(SavedEvent::Close),
            _ => return,
        }
        cx.stop_propagation();
    }

    // ----- Drawing -----

    fn header(&self, theme: &Theme) -> AnyElement {
        let colors = theme.colors;
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .h(px(52.))
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(theme.type_scale.title)
                    .text_color(colors.text)
                    .child("Saved"),
            )
            .child(
                div()
                    .debug_selector(|| "saved-hint".into())
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(theme.type_scale.small)
                    .text_color(if self.opening {
                        colors.text
                    } else {
                        colors.text_faint
                    })
                    .child(if self.opening {
                        "Finding the message…"
                    } else {
                        "Enter opens · Delete removes"
                    }),
            )
            // Room for the Close button, which sits over the right end.
            .child(div().w(px(76.)))
            .into_any_element()
    }

    fn rows_for(
        &mut self,
        range: std::ops::Range<usize>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let colors = theme.colors;
        let now = Zoned::now();
        let this = cx.entity().downgrade();
        range
            .filter_map(|index| self.rows.get(index).map(|row| (index, row.clone())))
            .map(|(index, row)| {
                let active = self.active == Some(index);
                let to = this.clone();
                let remove_to = this.clone();
                let written = ago_label(&to_zoned(row.written, &self.zone), &now);
                let saved = ago_label(&to_zoned(row.saved, &self.zone), &now);
                let label = format!("{}. {}: {}", row.place, row.author, row.snippet);
                div()
                    .id(("saved-row", index))
                    .debug_selector(move || format!("saved-row-{index}"))
                    .role(Role::ListItem)
                    .aria_label(label)
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .px_3()
                    .py_0p5()
                    .rounded(theme.radii.medium)
                    .cursor_pointer()
                    .when(!active, |d| d.hover(move |s| s.bg(colors.surface_hover)))
                    .on_click(move |_, _, cx| {
                        to.update(cx, |view, cx| {
                            view.active = Some(index);
                            view.open(index, cx)
                        })
                        .ok();
                    })
                    .child(
                        div()
                            .h_full()
                            .w_full()
                            .px_3()
                            .py_2()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .rounded(theme.radii.medium)
                            .border_1()
                            .border_color(if active {
                                colors.focus_ring
                            } else {
                                transparent_black()
                            })
                            .when(active, |d| d.bg(colors.selection))
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .text_size(theme.type_scale.small)
                                    .line_height(px(18.))
                                    .child(
                                        div()
                                            .truncate()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(colors.text)
                                            .child(row.place.clone()),
                                    )
                                    .children(row.outside.then(|| {
                                        div()
                                            .flex_none()
                                            .px_1p5()
                                            .rounded(theme.radii.small)
                                            .border_1()
                                            .border_color(colors.border_strong)
                                            .text_color(colors.text_muted)
                                            .child("Not joined")
                                    }))
                                    .children(row.reply.then(|| {
                                        div()
                                            .flex_none()
                                            .text_color(colors.text_muted)
                                            .child("Reply")
                                    }))
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_color(colors.text_muted)
                                            .child(format!("Saved {saved}")),
                                    )
                                    .child(
                                        div()
                                            .id(("saved-remove", index))
                                            .debug_selector(move || format!("saved-remove-{index}"))
                                            .role(Role::Button)
                                            .aria_label(states::REMOVE_FROM_SAVED)
                                            .flex_none()
                                            .px_1p5()
                                            .rounded(theme.radii.small)
                                            .border_1()
                                            .border_color(colors.border_strong)
                                            .text_color(colors.text)
                                            .cursor_pointer()
                                            .hover(move |s| s.bg(colors.surface_hover))
                                            .on_click(move |_, _, cx| {
                                                cx.stop_propagation();
                                                remove_to
                                                    .update(cx, |view, cx| view.remove(index, cx))
                                                    .ok();
                                            })
                                            .child("Remove"),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .truncate()
                                    .text_size(theme.type_scale.small)
                                    .line_height(px(18.))
                                    .text_color(colors.text_muted)
                                    .child(format!("{} · {written}", row.author)),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .h(px(42.))
                                    .overflow_hidden()
                                    .line_clamp(2)
                                    .text_size(theme.type_scale.body)
                                    .line_height(theme.type_scale.line_height_body)
                                    .text_color(colors.text)
                                    .child(row.snippet.clone()),
                            ),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn body(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let (status, count, has_more) = {
            let view = self.model.read(cx).core().saved();
            (view.status.clone(), self.rows.len(), view.has_more)
        };
        let this = cx.entity().downgrade();
        let retry_button = |view: &Self, label: &'static str| {
            let to = this.clone();
            form::button(
                theme,
                "saved-retry",
                label,
                ButtonKind::Primary,
                true,
                &view.retry_focus,
                move |window, cx| {
                    to.update(cx, |view, cx| view.retry(window, cx)).ok();
                },
            )
            .into_any_element()
        };
        match (&status, count) {
            (LoadStatus::Loading | LoadStatus::Idle, 0) => skeleton(theme),
            (LoadStatus::Failed { reason, can_retry }, 0) => note_block(
                theme,
                "saved-failed",
                Some(states::SAVED_FAILED_TITLE),
                states::sentence(reason),
                can_retry.then(|| retry_button(self, states::TRY_AGAIN)),
            ),
            (LoadStatus::Ready, 0) => note_block(
                theme,
                "saved-empty",
                None,
                states::SAVED_EMPTY.to_owned(),
                None,
            ),
            _ => {
                let list = uniform_list(
                    "saved-items",
                    count,
                    cx.processor(
                        |this,
                         range: std::ops::Range<usize>,
                         _window: &mut Window,
                         cx: &mut Context<Self>| {
                            let theme = *Theme::get(cx);
                            this.rows_for(range, &theme, cx)
                        },
                    ),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .min_h_0();
                let footer: Option<AnyElement> = match &status {
                    LoadStatus::Loading => Some(
                        div()
                            .debug_selector(|| "saved-loading-more".into())
                            .text_size(theme.type_scale.small)
                            .text_color(colors.text_muted)
                            .child("Loading more saved messages…")
                            .into_any_element(),
                    ),
                    LoadStatus::Failed { reason, can_retry } => Some(
                        div()
                            .debug_selector(|| "saved-more-failed".into())
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.danger)
                                    .child(states::saved_more_failed(reason)),
                            )
                            .children(can_retry.then(|| retry_button(self, states::TRY_AGAIN)))
                            .into_any_element(),
                    ),
                    LoadStatus::Ready | LoadStatus::Idle if has_more => {
                        let to = this.clone();
                        Some(
                            form::button(
                                theme,
                                "saved-show-more",
                                states::SEARCH_SHOW_MORE,
                                ButtonKind::Secondary,
                                true,
                                &self.more_focus,
                                move |_, cx| {
                                    to.update(cx, |view, cx| {
                                        view.dispatch(Input::LoadMoreSaved, cx)
                                    })
                                    .ok();
                                },
                            )
                            .into_any_element(),
                        )
                    }
                    _ => None,
                };
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("saved-list")
                            .debug_selector(|| "saved-list".into())
                            .role(Role::List)
                            .aria_label("Saved messages")
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .child(list),
                    )
                    .children(footer.map(|footer| {
                        div()
                            .flex_none()
                            .px_4()
                            .py_2()
                            .border_t_1()
                            .border_color(colors.border)
                            .child(footer)
                    }))
                    .into_any_element()
            }
        }
    }
}

fn row_of(core: &ClientCore, item: &SavedItem) -> Row {
    let message = &item.message;
    Row {
        conversation: item.conversation.id,
        message: message.id,
        place: place_label(core, &item.conversation),
        author: core.display_name(message.author_id).to_owned(),
        written: message.created_at,
        saved: item.saved_at,
        snippet: excerpt(message),
        reply: message.thread_root.is_some(),
        outside: core.conversation(item.conversation.id).is_none(),
    }
}

/// The first page loading: rows the shape of saved messages, so nothing moves when they arrive.
fn skeleton(theme: &Theme) -> AnyElement {
    let colors = theme.colors;
    let bar = |width: f32| {
        div()
            .h(px(12.))
            .w(px(width))
            .max_w_full()
            .rounded(theme.radii.small)
            .bg(colors.border)
    };
    div()
        .id("saved-loading")
        .debug_selector(|| "saved-loading".into())
        .role(Role::Status)
        .aria_label("Loading saved messages")
        .flex_1()
        .min_h_0()
        .overflow_hidden()
        .children((0..SKELETON_ROWS).map(|index| {
            let wide = 320.0 + (index % 3) as f32 * 80.0;
            div().h(px(ROW_HEIGHT)).w_full().px_3().py_0p5().child(
                div()
                    .h_full()
                    .px_3()
                    .py_2()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(bar(150.0))
                    .child(bar(110.0))
                    .child(bar(wide))
                    .child(bar(wide - 100.0)),
            )
        }))
        .into_any_element()
}

impl Focusable for SavedMessagesView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.list_focus.clone()
    }
}

impl Render for SavedMessagesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let header = self.header(&theme);
        let body = self.body(&theme, cx);
        let this = cx.entity().downgrade();
        let close = div()
            .absolute()
            .top(px(8.))
            .right(px(16.))
            .child(form::button(
                &theme,
                "saved-close",
                "Close",
                ButtonKind::Secondary,
                true,
                &self.close_focus,
                move |_, cx| {
                    this.update(cx, |_, cx| cx.emit(SavedEvent::Close)).ok();
                },
            ));
        div()
            .id("saved-view")
            .debug_selector(|| "saved-view".into())
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.colors.surface)
            .on_key_down(cx.listener(Self::on_key_down))
            .child(header)
            // The list's tab stop is the whole body, which every state has: while the first page
            // loads, or when there is nothing to list, the keyboard still has somewhere to be.
            .child(
                div()
                    .id("saved-body")
                    .debug_selector(|| "saved-body".into())
                    .track_focus(&self.list_focus)
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .border_1()
                    .border_color(transparent_black())
                    .focus_visible(move |style| style.border_color(theme.colors.focus_ring))
                    .child(body),
            )
            .child(close)
    }
}
