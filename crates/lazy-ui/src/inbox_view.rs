//! The thread inbox: the threads the person follows, most recently active first, in place of the
//! conversation.
//!
//! Each row says where the thread is, what its first message said, the newest reply and when, how
//! many replies it has, and how many are unread. Enter opens the thread in the pane, which the
//! inbox leaves in place beside it (at narrow widths the pane replaces it, by the pane's rule).
//!
//! # Keyboard
//!
//! The list is one tab stop and starts with the keyboard: Up and Down move the active row, Page
//! Up and Page Down by a screenful, Home and End to the first and last, Enter opens the active
//! thread, Escape closes the inbox. Moving to the last row asks for the next page. Tab order:
//! Unread/All (Left and Right choose), Mark all read, the list, Show more or Try again, and Close.

use gpui::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    KeyDownEvent, ParentElement, Render, Role, ScrollStrategy, Styled, Subscription,
    UniformListScrollHandle, Window, div, prelude::*, px, transparent_black, uniform_list,
};
use jiff::Zoned;
use jiff::tz::TimeZone;
use lazy_client_core::{Change, ClientCore, InboxStatus, Input};
use lazy_domain::{ConversationId, MessageId, Timestamp};
use lazy_protocol::{InboxEntry, Message};

use crate::app_model::{AppEvent, AppModel};
use crate::form::{self, ButtonKind};
use crate::states;
use crate::theme::Theme;
use crate::time_fmt::{ago_label, to_zoned};

/// The height of a thread row, so the list never re-measures.
pub const ROW_HEIGHT: f32 = 116.0;
const SKELETON_ROWS: usize = 6;
const FALLBACK_PAGE: usize = 4;
/// The longest an excerpt is, in characters, before it is cut with an ellipsis.
const EXCERPT_CHARS: usize = 160;

/// What the inbox asks the shell to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxEvent {
    /// Escape or Close: leave the inbox and give the keyboard back.
    Close,
    /// Enter or a click on a thread: open it in the pane.
    OpenThread {
        conversation: ConversationId,
        root: MessageId,
    },
}

/// One thread, ready to draw.
#[derive(Debug, Clone)]
struct Row {
    conversation: ConversationId,
    root: MessageId,
    place: String,
    joined: bool,
    root_author: String,
    /// `None` when the first message was deleted.
    root_excerpt: Option<String>,
    last: Option<(String, String, Timestamp)>,
    replies: u32,
    unread: u32,
    mentions: u32,
}

/// What a row says, for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowInfo {
    pub place: String,
    pub root_author: String,
    pub root_excerpt: Option<String>,
    pub last_reply: Option<(String, String)>,
    pub replies: u32,
    pub unread: u32,
}

pub struct ThreadInboxView {
    model: Entity<AppModel>,
    scroll: UniformListScrollHandle,
    active: Option<usize>,
    rows: Vec<Row>,
    rows_generation: u64,
    zone: TimeZone,
    list_focus: FocusHandle,
    toggle_focus: FocusHandle,
    mark_focus: FocusHandle,
    more_focus: FocusHandle,
    retry_focus: FocusHandle,
    close_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<InboxEvent> for ThreadInboxView {}

impl ThreadInboxView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe_in(
            &model,
            window,
            |this, _model, event: &AppEvent, _window, cx| {
                let AppEvent::Changed(changes) = event;
                if changes
                    .iter()
                    .any(|change| matches!(change, Change::InboxChanged))
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
            rows_generation: u64::MAX,
            zone: TimeZone::system(),
            list_focus: cx.focus_handle().tab_stop(true),
            toggle_focus: cx.focus_handle().tab_stop(true),
            mark_focus: cx.focus_handle().tab_stop(true),
            more_focus: cx.focus_handle().tab_stop(true),
            retry_focus: cx.focus_handle().tab_stop(true),
            close_focus: cx.focus_handle().tab_stop(true),
            _subscriptions: vec![subscription],
        };
        view.sync_rows(cx);
        view
    }

    // ----- For the shell and tests -----

    /// What each thread row says. For tests.
    pub fn rows_info(&self) -> Vec<RowInfo> {
        self.rows
            .iter()
            .map(|row| RowInfo {
                place: row.place.clone(),
                root_author: row.root_author.clone(),
                root_excerpt: row.root_excerpt.clone(),
                last_reply: row
                    .last
                    .as_ref()
                    .map(|(who, text, _)| (who.clone(), text.clone())),
                replies: row.replies,
                unread: row.unread,
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
        } else if self.toggle_focus.is_focused(window) {
            "toggle"
        } else if self.mark_focus.is_focused(window) {
            "mark"
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
        self.rows_generation = u64::MAX;
        window.focus(&self.list_focus, cx);
        self.sync_rows(cx);
        cx.notify();
    }

    // ----- Reading the core -----

    fn dispatch(&self, input: Input, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
    }

    fn sync_rows(&mut self, cx: &mut Context<Self>) {
        let core = self.model.read(cx).core();
        let Some(view) = core.inbox() else {
            self.rows.clear();
            self.rows_generation = u64::MAX;
            return;
        };
        if self.rows_generation == view.generation && self.rows.len() == view.entries.len() {
            // The same page, though unread counts may have moved.
            self.rows = view
                .entries
                .iter()
                .map(|entry| row_of(core, entry))
                .collect();
            return;
        }
        self.rows = view
            .entries
            .iter()
            .map(|entry| row_of(core, entry))
            .collect();
        self.rows_generation = view.generation;
        // The list may shrink back to one page after a quiet refetch: keep the choice in range.
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
        let more = self
            .model
            .read(cx)
            .core()
            .inbox()
            .is_some_and(|view| view.has_more && matches!(view.status, InboxStatus::Ready));
        if more {
            self.dispatch(Input::LoadMoreInbox, cx);
        }
    }

    fn open(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get(index) {
            cx.emit(InboxEvent::OpenThread {
                conversation: row.conversation,
                root: row.root,
            });
        }
    }

    fn set_unread_only(&mut self, unread_only: bool, cx: &mut Context<Self>) {
        if self
            .model
            .read(cx)
            .core()
            .inbox()
            .is_some_and(|view| view.unread_only != unread_only)
        {
            self.active = None;
            self.dispatch(Input::LoadInbox { unread_only }, cx);
            cx.notify();
        }
    }

    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The button it was on goes away: the keyboard moves to the list, which is always there.
        window.focus(&self.list_focus, cx);
        let (unread_only, has_rows) = self
            .model
            .read(cx)
            .core()
            .inbox()
            .map_or((false, false), |view| {
                (view.unread_only, !view.entries.is_empty())
            });
        if has_rows {
            self.dispatch(Input::LoadMoreInbox, cx);
        } else {
            self.dispatch(Input::LoadInbox { unread_only }, cx);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
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
            "enter" if self.list_focus.is_focused(_window) && !event.is_held => {
                if let Some(index) = self.active {
                    self.open(index, cx);
                }
            }
            "escape" => cx.emit(InboxEvent::Close),
            _ => return,
        }
        cx.stop_propagation();
    }

    // ----- Drawing -----

    fn header(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let (unread_only, marking, any_unread) = {
            let core = self.model.read(cx).core();
            (
                core.inbox().is_some_and(|view| view.unread_only),
                core.inbox().is_some_and(|view| view.marking_read),
                core.inbox_unread_total() > 0,
            )
        };
        let option = |label: &'static str, value: bool, selector: &'static str| {
            let chosen = unread_only == value;
            let to = this.clone();
            div()
                .id(selector)
                .debug_selector(move || selector.into())
                .role(Role::RadioButton)
                .aria_label(label)
                .px_2()
                .py_0p5()
                .rounded(theme.radii.small)
                .text_size(theme.type_scale.small)
                .cursor_pointer()
                .when(chosen, |d| {
                    d.bg(colors.surface_raised)
                        .border_1()
                        .border_color(colors.border_strong)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.text)
                })
                .when(!chosen, |d| d.text_color(colors.text_muted))
                .on_click(move |_, _, cx| {
                    to.update(cx, |view, cx| view.set_unread_only(value, cx))
                        .ok();
                })
                .child(label)
        };
        let key_to = this.clone();
        let mark_to = this.clone();
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
                    .child("Threads"),
            )
            .child(
                div()
                    .id("inbox-toggle")
                    .debug_selector(|| "inbox-toggle".into())
                    .role(Role::RadioGroup)
                    .aria_label("Which threads")
                    .track_focus(&self.toggle_focus)
                    .flex()
                    .items_center()
                    .gap_0p5()
                    .p_0p5()
                    .rounded(theme.radii.medium)
                    .bg(colors.surface_hover)
                    .border_1()
                    .border_color(transparent_black())
                    .focus_visible(move |style| style.border_color(colors.focus_ring))
                    .on_key_down(move |event, _, cx| {
                        if event.keystroke.modifiers.modified() {
                            return;
                        }
                        let value = match event.keystroke.key.as_str() {
                            "left" => false,
                            "right" => true,
                            "enter" | "space" if !event.is_held => !unread_only,
                            _ => return,
                        };
                        key_to
                            .update(cx, |view, cx| view.set_unread_only(value, cx))
                            .ok();
                        cx.stop_propagation();
                    })
                    .child(option("All", false, "inbox-all"))
                    .child(option("Unread", true, "inbox-unread")),
            )
            .child(div().flex_1())
            .child(form::button(
                theme,
                "inbox-mark-read",
                "Mark all read",
                ButtonKind::Secondary,
                any_unread && !marking,
                &self.mark_focus,
                move |_, cx| {
                    mark_to
                        .update(cx, |view, cx| view.dispatch(Input::MarkInboxRead, cx))
                        .ok();
                },
            ))
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
                let unread = row.unread > 0;
                let when = row
                    .last
                    .as_ref()
                    .map(|(_, _, at)| ago_label(&to_zoned(*at, &self.zone), &now));
                let root_line = match &row.root_excerpt {
                    Some(text) => format!("{}: {text}", row.root_author),
                    None => states::INBOX_DELETED_ROOT.to_owned(),
                };
                let label = format!("{}. {root_line}", row.place);
                div()
                    .id(("inbox-thread", index))
                    .debug_selector(move || format!("inbox-thread-{index}"))
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
                                            .font_weight(if unread {
                                                FontWeight::BOLD
                                            } else {
                                                FontWeight::SEMIBOLD
                                            })
                                            .text_color(colors.text)
                                            .child(row.place.clone()),
                                    )
                                    .children((!row.joined).then(|| {
                                        div()
                                            .flex_none()
                                            .px_1p5()
                                            .rounded(theme.radii.small)
                                            .border_1()
                                            .border_color(colors.border_strong)
                                            .text_color(colors.text_muted)
                                            .child("Not joined")
                                    }))
                                    .child(div().flex_1())
                                    .children(when.map(|when| {
                                        div().flex_none().text_color(colors.text_muted).child(when)
                                    }))
                                    .children(unread.then(|| {
                                        div()
                                            .flex_none()
                                            .px_1p5()
                                            .rounded(theme.radii.small)
                                            .bg(if row.mentions > 0 {
                                                colors.danger
                                            } else {
                                                colors.accent
                                            })
                                            .text_color(colors.on_accent)
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(row.unread.to_string())
                                    })),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .truncate()
                                    .text_size(theme.type_scale.body)
                                    .line_height(theme.type_scale.line_height_body)
                                    .text_color(if row.root_excerpt.is_some() {
                                        colors.text
                                    } else {
                                        colors.text_muted
                                    })
                                    .child(root_line),
                            )
                            .children(row.last.clone().map(|(who, text, _)| {
                                div()
                                    .flex_none()
                                    .truncate()
                                    .text_size(theme.type_scale.body)
                                    .line_height(theme.type_scale.line_height_body)
                                    .text_color(colors.text_muted)
                                    .child(format!("{who}: {text}"))
                            }))
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(theme.type_scale.small)
                                    .line_height(px(18.))
                                    .text_color(colors.text_faint)
                                    .child(states::replies_label(row.replies)),
                            ),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn body(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let Some((status, count, unread_only, has_more)) =
            self.model.read(cx).core().inbox().map(|view| {
                (
                    view.status.clone(),
                    view.entries.len(),
                    view.unread_only,
                    view.has_more,
                )
            })
        else {
            return div().into_any_element();
        };
        let this = cx.entity().downgrade();
        let retry_button = |view: &Self, label: &'static str| {
            let to = this.clone();
            form::button(
                theme,
                "inbox-retry",
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
            (InboxStatus::Loading | InboxStatus::Idle, 0) => skeleton(theme),
            (InboxStatus::Failed { reason, can_retry }, 0) => note_block(
                theme,
                "inbox-failed",
                Some(states::INBOX_FAILED_TITLE),
                states::sentence(reason),
                can_retry.then(|| retry_button(self, states::TRY_AGAIN)),
            ),
            (InboxStatus::Ready, 0) => note_block_with(
                theme,
                "inbox-empty",
                crate::brand::moss(cx, crate::brand::Moss::CaughtUp, 120.)
                    .map(IntoElement::into_any_element),
                Some(states::INBOX_CAUGHT_UP),
                if unread_only {
                    states::INBOX_NO_UNREAD.to_owned()
                } else {
                    states::INBOX_EMPTY.to_owned()
                },
                None,
            ),
            _ => {
                let list = uniform_list(
                    "inbox-threads",
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
                    InboxStatus::Loading => Some(
                        div()
                            .debug_selector(|| "inbox-loading-more".into())
                            .text_size(theme.type_scale.small)
                            .text_color(colors.text_muted)
                            .child("Loading more threads…")
                            .into_any_element(),
                    ),
                    InboxStatus::Failed { reason, can_retry } => Some(
                        div()
                            .debug_selector(|| "inbox-more-failed".into())
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.danger)
                                    .child(states::inbox_more_failed(reason)),
                            )
                            .children(can_retry.then(|| retry_button(self, states::TRY_AGAIN)))
                            .into_any_element(),
                    ),
                    InboxStatus::Ready | InboxStatus::Idle if has_more => {
                        let to = this.clone();
                        Some(
                            form::button(
                                theme,
                                "inbox-show-more",
                                states::SEARCH_SHOW_MORE,
                                ButtonKind::Secondary,
                                true,
                                &self.more_focus,
                                move |_, cx| {
                                    to.update(cx, |view, cx| {
                                        view.dispatch(Input::LoadMoreInbox, cx)
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
                            .id("inbox-list")
                            .debug_selector(|| "inbox-list".into())
                            .role(Role::List)
                            .aria_label("Followed threads")
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

/// The first line of a message's text, cut to a readable length.
pub(crate) fn excerpt(message: &Message) -> String {
    let text = message.document.plain_text();
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() > EXCERPT_CHARS {
        let cut: String = line.chars().take(EXCERPT_CHARS).collect();
        format!("{}…", cut.trim_end())
    } else if message.document.has_no_content() && !message.attachments.is_empty() {
        message
            .attachments
            .first()
            .map(|file| file.name.clone())
            .unwrap_or_default()
    } else {
        line.to_owned()
    }
}

fn row_of(core: &ClientCore, entry: &InboxEntry) -> Row {
    let root = &entry.root;
    Row {
        conversation: entry.conversation.id,
        root: root.id,
        place: core.inbox_entry_title(entry),
        joined: core.inbox_entry_joined(entry),
        root_author: core.display_name(root.author_id).to_owned(),
        root_excerpt: (!root.deleted).then(|| excerpt(root)),
        last: entry.last_reply.as_ref().map(|reply| {
            (
                core.display_name(reply.author_id).to_owned(),
                excerpt(reply),
                reply.created_at,
            )
        }),
        replies: root.thread.as_ref().map_or(0, |thread| thread.reply_count),
        unread: entry.state.unread_count,
        mentions: entry.state.mention_count,
    }
}

/// A centered note with an optional heading and button.
pub(crate) fn note_block(
    theme: &Theme,
    selector: &'static str,
    title: Option<&'static str>,
    text: String,
    action: Option<AnyElement>,
) -> AnyElement {
    note_block_with(theme, selector, None, title, text, action)
}

/// Like [`note_block`], with a picture above the heading: Moss, for the "nothing left" states.
pub(crate) fn note_block_with(
    theme: &Theme,
    selector: &'static str,
    mascot: Option<AnyElement>,
    title: Option<&'static str>,
    text: String,
    action: Option<AnyElement>,
) -> AnyElement {
    let colors = theme.colors;
    div()
        .id(selector)
        .debug_selector(move || selector.into())
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .px_6()
        .py_8()
        .child(
            div()
                .max_w(px(460.))
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .text_center()
                .children(mascot)
                .children(title.map(|title| {
                    crate::fonts::heading(
                        div()
                            .text_size(theme.type_scale.title)
                            .text_color(colors.text)
                            .child(title),
                    )
                }))
                .child(
                    div()
                        .text_size(theme.type_scale.body)
                        .text_color(colors.text_muted)
                        .child(text),
                )
                .children(action),
        )
        .into_any_element()
}

/// The first page loading: rows the shape of threads, so nothing moves when they arrive.
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
        .id("inbox-loading")
        .debug_selector(|| "inbox-loading".into())
        .role(Role::Status)
        .aria_label("Loading threads")
        .flex_1()
        .min_h_0()
        .overflow_hidden()
        .children((0..SKELETON_ROWS).map(|index| {
            let wide = 300.0 + (index % 3) as f32 * 80.0;
            div().h(px(ROW_HEIGHT)).w_full().px_3().py_0p5().child(
                div()
                    .h_full()
                    .px_3()
                    .py_2()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(bar(140.0))
                    .child(bar(wide))
                    .child(bar(wide - 80.0))
                    .child(bar(70.0)),
            )
        }))
        .into_any_element()
}

impl Focusable for ThreadInboxView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.list_focus.clone()
    }
}

impl Render for ThreadInboxView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let header = self.header(&theme, cx);
        let body = self.body(&theme, cx);
        let this = cx.entity().downgrade();
        let close = div()
            .absolute()
            .top(px(8.))
            .right(px(16.))
            .child(form::button(
                &theme,
                "inbox-close",
                "Close",
                ButtonKind::Secondary,
                true,
                &self.close_focus,
                move |_, cx| {
                    this.update(cx, |_, cx| cx.emit(InboxEvent::Close)).ok();
                },
            ));
        div()
            .id("inbox-view")
            .debug_selector(|| "inbox-view".into())
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
                    .id("inbox-body")
                    .debug_selector(|| "inbox-body".into())
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
