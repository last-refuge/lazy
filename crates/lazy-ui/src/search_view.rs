//! The search view: the field, the filters the query was understood as, the sort, and the
//! results, in place of the conversation.
//!
//! The person types into the field; the core records and parses the text at once (so the chips
//! and a parse error appear as they type) and asks nothing. This view waits for a pause in
//! typing and then asks the core to run the search, because the core has no timers. Enter asks at
//! once. The core makes sure only the answer to the last request is shown.
//!
//! # Keyboard
//!
//! The field keeps the keyboard while the results are moved through: Up and Down move the active
//! result (or recent search), Page Up and Page Down move by a screenful, Ctrl+Home and Ctrl+End
//! are for the first and last when the list itself has the keyboard; from the field, which keeps Home, End and Ctrl+Home and Ctrl+End for its caret, Alt+Home and Alt+End go to the first and last. Enter opens the active result and with none active
//! searches now, and Escape first clears the active result and then closes the search. Nothing
//! acts while an input method is composing.
//!
//! Tab goes from the field to the sort control (Left and Right choose, Enter or Space toggles),
//! then each filter chip's remove button, then the results (one stop, with the same keys as the
//! field), then "Show more" or "Try again", then Close.
//!
//! Nothing here reads a file or the network. Rows are cut into highlighted pieces when the
//! results arrive, not when they are drawn.

use std::time::Duration;

use gpui::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    KeyDownEvent, ParentElement, Render, Role, ScrollStrategy, SharedString, Styled, StyledText,
    Subscription, Task, TextRun, UniformListScrollHandle, Window, div, prelude::*, px,
    transparent_black, uniform_list,
};
use jiff::Zoned;
use jiff::tz::TimeZone;
use lazy_client_core::{Change, ClientCore, Input, SearchStatus, SearchView};
use lazy_domain::Timestamp;
use lazy_protocol::{SearchHitKind, SearchSort};

use crate::app_model::{AppEvent, AppModel};
use crate::emoji_font;
use crate::fonts;
use crate::form::{self, ButtonKind};
use crate::search_text::{self, Chip, Piece};
use crate::states;
use crate::text_input::{TextInput, TextInputEvent, TextInputOptions};
use crate::theme::Theme;
use crate::time_fmt::{ago_label, to_zoned};

/// How long after the last keystroke the search runs.
pub const DEBOUNCE: Duration = Duration::from_millis(250);
/// The height of a result row, so the list never re-measures and loading rows take the same room.
pub const ROW_HEIGHT: f32 = 92.0;
/// The height of a recent search row.
const RECENT_HEIGHT: f32 = 36.0;
/// Rows of skeleton shown while the first page loads.
const SKELETON_ROWS: usize = 8;
/// Rows a page keys move by when the list has not been measured yet.
const FALLBACK_PAGE: usize = 5;

/// What the view asks the shell to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchEvent {
    /// Escape, or the Close button: leave search and give the keyboard back.
    Close,
    /// Open result `index`: show its conversation (or its thread, for a reply) and give the
    /// keyboard to it. The shell asks the core and closes the search.
    Open { index: usize, reply: bool },
}

/// One result, ready to draw.
#[derive(Debug, Clone)]
struct Row {
    place: String,
    joined: bool,
    author: String,
    created_at: Timestamp,
    reply: bool,
    file: bool,
    snippet: SharedString,
    pieces: Vec<Piece>,
}

/// What a row says, for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowInfo {
    pub place: String,
    pub joined: bool,
    pub author: String,
    pub reply: bool,
    pub file: bool,
    pub snippet: String,
    /// The pieces of the snippet that are highlighted, in order.
    pub marked: Vec<String>,
}

pub struct SearchResultsView {
    model: Entity<AppModel>,
    field: Entity<TextInput>,
    scroll: UniformListScrollHandle,
    /// The result (or, with an empty field, the recent search) the keyboard is on.
    active: Option<usize>,
    /// Waiting to run the search once typing pauses.
    debounce: Option<Task<()>>,
    /// The results of the core's view, cut up for drawing, and the generation they are from.
    rows: Vec<Row>,
    rows_generation: u64,
    /// A result was chosen and the core is still finding the message.
    opening: bool,
    zone: TimeZone,
    sort_focus: FocusHandle,
    chip_focus: Vec<FocusHandle>,
    list_focus: FocusHandle,
    more_focus: FocusHandle,
    retry_focus: FocusHandle,
    close_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SearchEvent> for SearchResultsView {}

impl SearchResultsView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let field = cx.new(|cx| {
            TextInput::new(
                TextInputOptions::new("Search")
                    .accessible_label("Search messages and files")
                    .single_line(true)
                    .max_bytes(1_000)
                    .pass_through_navigation(true),
                cx,
            )
        });
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &field,
            window,
            |this, _field, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Changed => this.typed(cx),
                TextInputEvent::Submit => this.submit(window, cx),
                TextInputEvent::Tab { backwards } => {
                    if *backwards {
                        window.focus_prev(cx)
                    } else {
                        window.focus_next(cx)
                    }
                }
                TextInputEvent::UpWhenEmpty
                | TextInputEvent::PasteAttachments
                | TextInputEvent::Suggestion(_) => {}
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &model,
            window,
            |this, _model, event: &AppEvent, _window, cx| {
                let AppEvent::Changed(changes) = event;
                if changes
                    .iter()
                    .any(|change| matches!(change, Change::SearchChanged))
                {
                    this.sync_rows(cx);
                }
                cx.notify();
            },
        ));
        let mut view = Self {
            model,
            field,
            scroll: UniformListScrollHandle::new(),
            active: None,
            debounce: None,
            rows: Vec::new(),
            rows_generation: u64::MAX,
            opening: false,
            zone: TimeZone::system(),
            sort_focus: cx.focus_handle().tab_stop(true),
            chip_focus: Vec::new(),
            list_focus: cx.focus_handle().tab_stop(true),
            more_focus: cx.focus_handle().tab_stop(true),
            retry_focus: cx.focus_handle().tab_stop(true),
            close_focus: cx.focus_handle().tab_stop(true),
            _subscriptions: subscriptions,
        };
        view.sync_rows(cx);
        view
    }

    // ----- For the shell and tests -----

    /// The search field.
    pub fn field(&self) -> &Entity<TextInput> {
        &self.field
    }

    /// What each result row says. For tests.
    pub fn rows_info(&self) -> Vec<RowInfo> {
        self.rows
            .iter()
            .map(|row| RowInfo {
                place: row.place.clone(),
                joined: row.joined,
                author: row.author.clone(),
                reply: row.reply,
                file: row.file,
                snippet: row.snippet.to_string(),
                marked: row
                    .pieces
                    .iter()
                    .filter(|piece| piece.highlighted)
                    .map(|piece| row.snippet[piece.range.clone()].to_owned())
                    .collect(),
            })
            .collect()
    }

    /// Which of the view's tab stops has the keyboard. For tests.
    pub fn stop(&self, window: &Window, cx: &gpui::App) -> &'static str {
        if self.field.read(cx).is_focused(window) {
            "field"
        } else if self.sort_focus.is_focused(window) {
            "sort"
        } else if self
            .chip_focus
            .iter()
            .any(|handle| handle.is_focused(window))
        {
            "chip"
        } else if self.list_focus.is_focused(window) {
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

    /// The result the keyboard is on, if any.
    pub fn active(&self) -> Option<usize> {
        self.active
    }

    /// Starts afresh: an empty field with the keyboard in it.
    pub fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.debounce = None;
        self.active = None;
        self.field.update(cx, |field, cx| {
            field.set_text("", cx);
            field.focus(window, cx);
        });
        self.rows.clear();
        self.rows_generation = u64::MAX;
        self.opening = false;
        cx.notify();
    }

    /// Says a chosen result is being looked for (the message may have to be fetched).
    pub fn set_opening(&mut self, opening: bool, cx: &mut Context<Self>) {
        if self.opening != opening {
            self.opening = opening;
            cx.notify();
        }
    }

    /// Puts the keyboard in the field.
    pub fn focus_field(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.field.update(cx, |field, cx| field.focus(window, cx));
    }

    // ----- Reading the core -----

    fn view<'a>(&self, cx: &'a gpui::App) -> Option<&'a SearchView> {
        self.model.read(cx).core().search()
    }

    fn dispatch(&self, input: Input, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
    }

    /// Cuts the core's results into rows, once, when they change.
    fn sync_rows(&mut self, cx: &mut Context<Self>) {
        let core = self.model.read(cx).core();
        let Some(view) = core.search() else {
            self.rows.clear();
            self.rows_generation = u64::MAX;
            return;
        };
        if self.rows_generation == view.generation && self.rows.len() == view.hits.len() {
            return;
        }
        self.rows = rows_of(core, view);
        self.rows_generation = view.generation;
        self.active = self.active.filter(|at| *at < self.rows.len());
        // Fresh results start with nothing chosen.
        if self.rows.is_empty() || view.status == SearchStatus::Loading {
            self.active = None;
        }
    }

    // ----- Typing -----

    /// The text changed: the core parses it now, and the search runs after a pause.
    fn typed(&mut self, cx: &mut Context<Self>) {
        let text = self.field.read(cx).text().to_owned();
        self.active = None;
        self.dispatch(Input::SetSearchQuery { text: text.clone() }, cx);
        self.debounce = None;
        if text.trim().is_empty() {
            return;
        }
        self.debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            this.update(cx, |this, cx| this.pause(cx)).ok();
        }));
        cx.notify();
    }

    /// Typing paused: search, unless an input method is still composing.
    fn pause(&mut self, cx: &mut Context<Self>) {
        self.debounce = None;
        if self.field.read(cx).is_composing() {
            // The text is not final: look again after another pause.
            self.debounce = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(DEBOUNCE).await;
                this.update(cx, |this, cx| this.pause(cx)).ok();
            }));
            return;
        }
        self.dispatch(Input::RunSearch, cx);
        cx.notify();
    }

    /// Searches now (Enter, a recent search, a removed filter).
    fn run_now(&mut self, cx: &mut Context<Self>) {
        self.debounce = None;
        self.active = None;
        self.dispatch(Input::RunSearch, cx);
        cx.notify();
    }

    /// Replaces the text of the field and searches for it at once.
    fn search_for(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.field.update(cx, |field, cx| {
            field.set_text(text, cx);
            field.focus(window, cx);
        });
        self.dispatch(
            Input::SetSearchQuery {
                text: text.to_owned(),
            },
            cx,
        );
        self.run_now(cx);
    }

    fn remove_chip(&mut self, chip: &Chip, window: &mut Window, cx: &mut Context<Self>) {
        let text = search_text::remove_chip(self.field.read(cx).text(), chip);
        self.search_for(&text, window, cx);
    }

    fn set_sort(&mut self, sort: SearchSort, cx: &mut Context<Self>) {
        if self.view(cx).is_some_and(|view| view.sort != sort) {
            self.active = None;
            self.dispatch(Input::SetSearchSort(sort), cx);
            cx.notify();
        }
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        let more = self.view(cx).is_some_and(|view| !view.hits.is_empty());
        if more {
            self.dispatch(Input::LoadMoreSearch, cx);
        } else {
            self.run_now(cx);
        }
    }

    // ----- Moving through the list -----

    /// How many things the keys move through: recent searches with an empty field, else results.
    fn count(&self, cx: &gpui::App) -> usize {
        let core = self.model.read(cx).core();
        match core.search() {
            Some(view) if view.query.trim().is_empty() => core.recent_searches().len(),
            Some(_) => self.rows.len(),
            None => 0,
        }
    }

    fn page(&self) -> usize {
        let state = self.scroll.0.borrow();
        state
            .last_item_size
            .map(|size| (f32::from(size.item.height) / ROW_HEIGHT).floor() as usize)
            .filter(|rows| *rows > 0)
            .unwrap_or(FALLBACK_PAGE)
    }

    fn move_to(&mut self, index: usize, cx: &mut Context<Self>) {
        let count = self.count(cx);
        if count == 0 {
            return;
        }
        let index = index.min(count - 1);
        self.active = Some(index);
        self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
        // Reaching the last result asks for the next page.
        let at_end = index + 1 == count;
        if at_end
            && self.view(cx).is_some_and(|view| {
                !view.query.trim().is_empty()
                    && view.has_more
                    && !view.at_limit
                    && view.status == SearchStatus::Ready
            })
        {
            self.dispatch(Input::LoadMoreSearch, cx);
        }
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.count(cx);
        if count == 0 {
            return;
        }
        let next = match self.active {
            None if delta > 0 => 0,
            None => return,
            Some(at) => (at as isize + delta).clamp(0, count as isize - 1) as usize,
        };
        self.move_to(next, cx);
    }

    /// Enter: open the active result, or search now.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.field.read(cx).is_composing() {
            return;
        }
        match self.active {
            Some(index) => self.open(index, window, cx),
            None => self.run_now(cx),
        }
    }

    /// Opens result `index` (or runs the recent search at that place).
    fn open(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.opening {
            return;
        }
        let empty_field = self
            .view(cx)
            .is_some_and(|view| view.query.trim().is_empty());
        if empty_field {
            let recent = self
                .model
                .read(cx)
                .core()
                .recent_searches()
                .get(index)
                .cloned();
            if let Some(text) = recent {
                self.search_for(&text, window, cx);
            }
            return;
        }
        let Some(reply) = self
            .view(cx)
            .and_then(|view| view.hits.get(index))
            .map(|hit| hit.message.thread_root.is_some())
        else {
            return;
        };
        cx.emit(SearchEvent::Open { index, reply });
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.field.read(cx).is_composing() {
            return;
        }
        let modifiers = event.keystroke.modifiers;
        let plain = !modifiers.modified();
        let jump = (modifiers.control || modifiers.platform) && !modifiers.shift && !modifiers.alt;
        let in_field = self.field.read(cx).is_focused(window);
        let on_list = self.list_focus.is_focused(window);
        let alt = modifiers.alt && !modifiers.control && !modifiers.platform && !modifiers.shift;
        let first_last = alt || (on_list && (plain || jump));
        match event.keystroke.key.as_str() {
            "down" if plain => self.step(1, cx),
            "up" if plain => self.step(-1, cx),
            "pagedown" if plain => self.step(self.page() as isize, cx),
            "pageup" if plain => self.step(-(self.page() as isize), cx),
            // From the field Home and End and Ctrl+Home and Ctrl+End are the caret's, so Alt+Home
            // and Alt+End jump; on the list's own stop all of them do.
            "home" if first_last => self.move_to(0, cx),
            "end" if first_last => {
                let last = self.count(cx).saturating_sub(1);
                self.move_to(last, cx)
            }
            "escape" if plain => {
                if self.active.take().is_some() {
                    cx.notify();
                } else {
                    cx.emit(SearchEvent::Close);
                }
            }
            // In the field Enter is the field's own (Submit); on the list it is the same.
            "enter" if plain && !in_field && on_list => self.submit(window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    // ----- Drawing -----

    fn header(&mut self, theme: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let (parsed_error, chips, sort, has_text, can_sort) = match self.view(cx) {
            Some(view) => (
                match (&view.parsed, view.query.trim().is_empty()) {
                    (Err(error), false) => Some(states::search_error_text(error)),
                    _ => None,
                },
                view.parsed
                    .as_ref()
                    .map(search_text::chips)
                    .unwrap_or_default(),
                view.sort,
                // Only a query known to be filters alone has no order to choose; one that does not
                // parse keeps the order that was chosen.
                view.parsed.as_ref().map_or(true, |query| query.has_text()),
                !view.query.trim().is_empty(),
            ),
            None => (None, Vec::new(), SearchSort::Relevance, false, false),
        };
        while self.chip_focus.len() < chips.len() {
            self.chip_focus.push(cx.focus_handle().tab_stop(true));
        }
        let focused = self.field.read(cx).is_focused(window);
        let sort_row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(self.sort_control(theme, sort, has_text, can_sort, cx))
            .children(chips.iter().enumerate().map(|(index, chip)| {
                let remove_to = this.clone();
                let chip_to_remove = chip.clone();
                let label = chip.label();
                div()
                    .id(("search-chip", index))
                    .debug_selector(move || format!("search-chip-{index}"))
                    .flex()
                    .items_center()
                    .gap_1()
                    .pl_2()
                    .rounded(theme.radii.medium)
                    .bg(colors.accent_surface)
                    .border_1()
                    .border_color(colors.border_strong)
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text)
                    .child(label)
                    .child(
                        div()
                            .id(("search-chip-remove", index))
                            .debug_selector(move || format!("search-chip-remove-{index}"))
                            .role(Role::Button)
                            .aria_label(chip.remove_label())
                            .track_focus(&self.chip_focus[index])
                            .cursor_pointer()
                            .px_1p5()
                            .rounded(theme.radii.small)
                            .border_1()
                            .border_color(transparent_black())
                            .focus_visible(move |style| style.border_color(colors.focus_ring))
                            .hover(move |style| style.bg(colors.surface_hover))
                            .on_key_down({
                                let remove_to = remove_to.clone();
                                let chip = chip_to_remove.clone();
                                move |event, window, cx| {
                                    if crate::keyboard::is_activation(event) {
                                        let chip = chip.clone();
                                        remove_to
                                            .update(cx, |view, cx| {
                                                view.remove_chip(&chip, window, cx)
                                            })
                                            .ok();
                                        cx.stop_propagation();
                                    }
                                }
                            })
                            .on_click(move |_, window, cx| {
                                let chip = chip_to_remove.clone();
                                remove_to
                                    .update(cx, |view, cx| view.remove_chip(&chip, window, cx))
                                    .ok();
                            })
                            .child("×"),
                    )
            }));
        div()
            .relative()
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .px_4()
            .pt_3()
            .pb_2()
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    // Room for the Close button, which sits over the right end.
                    .pr(px(84.))
                    .child(
                        div()
                            .debug_selector(|| "search-field".into())
                            .flex_1()
                            .min_w_0()
                            .px_3()
                            .py_1p5()
                            .rounded(theme.radii.medium)
                            .bg(colors.surface_raised)
                            .border_1()
                            .border_color(if focused {
                                colors.focus_ring
                            } else {
                                colors.border_strong
                            })
                            .child(self.field.clone()),
                    ),
            )
            .children(parsed_error.map(|text| {
                div()
                    .id("search-error")
                    .debug_selector(|| "search-error".into())
                    .role(Role::Alert)
                    .text_size(theme.type_scale.small)
                    .text_color(colors.danger)
                    .child(text)
            }))
            .children(can_sort.then_some(sort_row))
            .into_any_element()
    }

    /// Best match or Newest: one stop; Left and Right choose, Enter or Space toggles.
    fn sort_control(
        &mut self,
        theme: &Theme,
        sort: SearchSort,
        has_text: bool,
        can_sort: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        // A search of only filters is always newest first: there is nothing to choose.
        let effective = if has_text { sort } else { SearchSort::Recent };
        let option = |label: &'static str, value: SearchSort, selector: &'static str| {
            let chosen = effective == value;
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
                .when(chosen, |d| {
                    d.bg(colors.surface_raised)
                        .border_1()
                        .border_color(colors.border_strong)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.text)
                })
                .when(!chosen, |d| d.text_color(colors.text_muted))
                .when(has_text, |d| {
                    d.cursor_pointer().on_click(move |_, _, cx| {
                        to.update(cx, |view, cx| view.set_sort(value, cx)).ok();
                    })
                })
                .child(label)
        };
        let key_to = this.clone();
        div()
            .id("search-sort")
            .debug_selector(|| "search-sort".into())
            .role(Role::RadioGroup)
            .aria_label("Sort results")
            .track_focus(&self.sort_focus)
            .flex()
            .items_center()
            .gap_0p5()
            .p_0p5()
            .rounded(theme.radii.medium)
            .bg(colors.surface_hover)
            .border_1()
            .border_color(transparent_black())
            .focus_visible(move |style| style.border_color(colors.focus_ring))
            .when(!can_sort, |d| d.opacity(0.6))
            .on_key_down(move |event, _window, cx| {
                if !has_text || event.keystroke.modifiers.modified() {
                    return;
                }
                let next = match event.keystroke.key.as_str() {
                    "left" => SearchSort::Relevance,
                    "right" => SearchSort::Recent,
                    "enter" | "space" if !event.is_held => match effective {
                        SearchSort::Relevance => SearchSort::Recent,
                        SearchSort::Recent => SearchSort::Relevance,
                    },
                    _ => return,
                };
                key_to.update(cx, |view, cx| view.set_sort(next, cx)).ok();
                cx.stop_propagation();
            })
            .child(option(
                "Best match",
                SearchSort::Relevance,
                "search-sort-best",
            ))
            .child(option("Newest", SearchSort::Recent, "search-sort-newest"))
            .into_any_element()
    }

    fn close_button(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        div()
            .absolute()
            .top(px(12.))
            .right(px(16.))
            .child(form::button(
                theme,
                "search-close",
                "Close",
                ButtonKind::Secondary,
                true,
                &self.close_focus,
                move |_, cx| {
                    this.update(cx, |_, cx| cx.emit(SearchEvent::Close)).ok();
                },
            ))
            .into_any_element()
    }

    fn body(&mut self, theme: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let Some(view) = self.view(cx) else {
            return div().into_any_element();
        };
        let empty_field = view.query.trim().is_empty();
        let status = view.status.clone();
        let hits = view.hits.len();
        if empty_field {
            return self.idle(theme, cx);
        }
        let typed = view.query.trim().to_owned();
        let filters: Vec<String> = view
            .parsed
            .as_ref()
            .map(|query| search_text::chips(query).iter().map(Chip::label).collect())
            .unwrap_or_default();
        match (&status, hits) {
            (SearchStatus::Idle, 0) => message_block(
                theme,
                "search-idle",
                None,
                states::SEARCH_IDLE.to_owned(),
                None,
            ),
            (SearchStatus::Loading, 0) => skeleton(theme),
            (SearchStatus::Ready, 0) => message_block(
                theme,
                "search-no-results",
                Some(states::SEARCH_NO_RESULTS_TITLE),
                states::search_no_results(&typed, &filters),
                None,
            ),
            (SearchStatus::Failed { reason, can_retry }, 0) => {
                let retry = (*can_retry).then(|| self.retry_button(theme, cx));
                let mut text = states::sentence(reason);
                if let Some(hint) = states::search_failed_hint(reason) {
                    text = format!("{text} {hint}");
                }
                message_block(
                    theme,
                    "search-failed",
                    Some(states::SEARCH_FAILED_TITLE),
                    text,
                    retry,
                )
            }
            _ => self.results(theme, window, cx),
        }
        .pipe(|element| {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .bg(colors.surface)
                .child(element)
                .into_any_element()
        })
    }

    fn retry_button(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        form::button(
            theme,
            "search-retry",
            states::TRY_AGAIN,
            ButtonKind::Primary,
            true,
            &self.retry_focus,
            move |_, cx| {
                this.update(cx, |view, cx| view.retry(cx)).ok();
            },
        )
        .into_any_element()
    }

    /// With an empty field: the recent searches, or what to type, and the filters.
    fn idle(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let recents: Vec<String> = self.model.read(cx).core().recent_searches().to_vec();
        let this = cx.entity().downgrade();
        let mut column = div()
            .id("search-idle-area")
            .debug_selector(|| "search-idle".into())
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .px_4()
            .py_3()
            .gap_1();
        if recents.is_empty() {
            column = column.child(
                div()
                    .text_size(theme.type_scale.body)
                    .text_color(colors.text)
                    .child(states::SEARCH_IDLE),
            );
        } else {
            column = column.child(
                div()
                    .id("search-recent-heading")
                    .pb_1()
                    .text_size(theme.type_scale.small)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.text_muted)
                    .child(states::RECENT_SEARCHES),
            );
            column = column.child(
                div()
                    .id("search-recents")
                    .debug_selector(|| "search-recents".into())
                    .role(Role::List)
                    .aria_label(states::RECENT_SEARCHES)
                    .track_focus(&self.list_focus)
                    .rounded(theme.radii.medium)
                    .border_1()
                    .border_color(transparent_black())
                    .focus_visible(move |style| style.border_color(colors.focus_ring))
                    .children(recents.iter().enumerate().map(|(index, text)| {
                        let to = this.clone();
                        let query = text.clone();
                        let active = self.active == Some(index);
                        div()
                            .id(("search-recent", index))
                            .debug_selector(move || format!("search-recent-{index}"))
                            .role(Role::ListItem)
                            .aria_label(text.clone())
                            .h(px(RECENT_HEIGHT))
                            .px_3()
                            .flex()
                            .items_center()
                            .gap_2()
                            .rounded(theme.radii.small)
                            .cursor_pointer()
                            .text_size(theme.type_scale.body)
                            .text_color(colors.text)
                            .border_1()
                            .border_color(if active {
                                colors.focus_ring
                            } else {
                                transparent_black()
                            })
                            .when(active, |d| d.bg(colors.selection))
                            .hover(move |style| style.bg(colors.surface_hover))
                            .on_click(move |_, window, cx| {
                                let query = query.clone();
                                to.update(cx, |view, cx| view.search_for(&query, window, cx))
                                    .ok();
                            })
                            .child(
                                div()
                                    .text_color(colors.text_faint)
                                    .text_size(theme.type_scale.small)
                                    .child("↺"),
                            )
                            .child(div().truncate().child(text.clone()))
                    })),
            );
        }
        column
            .child(
                div()
                    .id("search-filter-hint")
                    .debug_selector(|| "search-filter-hint".into())
                    .pt_2()
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_muted)
                    .child(states::SEARCH_FILTER_HINT),
            )
            .into_any_element()
    }

    /// The results, with the notes and the "Show more" row under them.
    fn results(
        &mut self,
        theme: &Theme,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let Some(view) = self.view(cx) else {
            return div().into_any_element();
        };
        let truncated = view.truncated && view.sort == SearchSort::Relevance;
        let (has_more, at_limit, status) = (view.has_more, view.at_limit, view.status.clone());
        let count = self.rows.len();
        let list = uniform_list(
            "search-results",
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
        let this = cx.entity().downgrade();
        let footer: Option<AnyElement> = match &status {
            SearchStatus::Loading => Some(
                div()
                    .id("search-loading-more")
                    .debug_selector(|| "search-loading-more".into())
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_muted)
                    .child(states::SEARCH_LOADING_MORE)
                    .into_any_element(),
            ),
            SearchStatus::Failed { reason, can_retry } => Some(
                div()
                    .id("search-more-failed")
                    .debug_selector(|| "search-more-failed".into())
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(theme.type_scale.small)
                            .text_color(colors.danger)
                            .child(states::search_more_failed(reason)),
                    )
                    .children((*can_retry).then(|| self.retry_button(theme, cx)))
                    .into_any_element(),
            ),
            SearchStatus::Ready | SearchStatus::Idle if at_limit => Some(
                div()
                    .id("search-at-limit")
                    .debug_selector(|| "search-at-limit".into())
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_muted)
                    .child(states::SEARCH_AT_LIMIT)
                    .into_any_element(),
            ),
            SearchStatus::Ready | SearchStatus::Idle if has_more => {
                let more_to = this.clone();
                Some(
                    form::button(
                        theme,
                        "search-show-more",
                        states::SEARCH_SHOW_MORE,
                        ButtonKind::Secondary,
                        true,
                        &self.more_focus,
                        move |_, cx| {
                            more_to
                                .update(cx, |view, cx| view.dispatch(Input::LoadMoreSearch, cx))
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
            .children(self.opening.then(|| {
                div()
                    .id("search-opening")
                    .debug_selector(|| "search-opening".into())
                    .role(Role::Status)
                    .flex_none()
                    .px_4()
                    .py_1p5()
                    .bg(colors.accent_surface)
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text)
                    .child(states::SEARCH_OPENING)
            }))
            .children(truncated.then(|| {
                div()
                    .id("search-truncated")
                    .debug_selector(|| "search-truncated".into())
                    .flex_none()
                    .px_4()
                    .py_1p5()
                    .bg(colors.warning_surface)
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text)
                    .child(states::SEARCH_TRUNCATED)
            }))
            .child(
                div()
                    .id("search-list")
                    .debug_selector(|| "search-list".into())
                    .role(Role::List)
                    .aria_label("Search results")
                    .track_focus(&self.list_focus)
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .border_1()
                    .border_color(transparent_black())
                    .focus_visible(move |style| style.border_color(colors.focus_ring))
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
                let when = ago_label(&to_zoned(row.created_at, &self.zone), &now);
                let label = format!("{} in {}: {}", row.author, row.place, row.snippet);
                div()
                    .id(("search-hit", index))
                    .debug_selector(move || format!("search-hit-{index}"))
                    .role(Role::ListItem)
                    .aria_label(label)
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .px_3()
                    .py_0p5()
                    .rounded(theme.radii.medium)
                    .cursor_pointer()
                    .when(!active, |d| d.hover(move |s| s.bg(colors.surface_hover)))
                    .on_click(move |_, window, cx| {
                        to.update(cx, |view, cx| view.open(index, window, cx)).ok();
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
                                    .child(
                                        div()
                                            .truncate()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(colors.text)
                                            .child(row.place.clone()),
                                    )
                                    .children((!row.joined).then(|| tag(theme, "Not joined")))
                                    .children(row.reply.then(|| tag(theme, "in thread")))
                                    .child(div().flex_1())
                                    .child(
                                        div().flex_none().text_color(colors.text_muted).child(when),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .flex()
                                    .gap_2()
                                    .text_size(theme.type_scale.body)
                                    .text_color(colors.text)
                                    .children(row.file.then(|| {
                                        div()
                                            .flex_none()
                                            .debug_selector(move || {
                                                format!("search-hit-file-{index}")
                                            })
                                            .child(emoji_font::glyphs("📎"))
                                    }))
                                    .child(div().min_w_0().flex_1().child(
                                        div().line_clamp(2).child(snippet_text(&row, theme)),
                                    )),
                            ),
                    )
                    .into_any_element()
            })
            .collect()
    }
}

/// A small pill beside where a result is.
fn tag(theme: &Theme, text: &'static str) -> AnyElement {
    let colors = theme.colors;
    div()
        .flex_none()
        .px_1p5()
        .rounded(theme.radii.small)
        .border_1()
        .border_color(colors.border_strong)
        .text_size(theme.type_scale.small)
        .text_color(colors.text_muted)
        .child(text)
        .into_any_element()
}

/// "Author: snippet" as one run of text, so it wraps and clamps as a paragraph, with the matches in
/// the snippet highlighted.
fn snippet_text(row: &Row, theme: &Theme) -> StyledText {
    let colors = theme.colors;
    let mut face = gpui::font(fonts::FAMILY);
    face.weight = FontWeight::NORMAL;
    let mut strong = face.clone();
    strong.weight = FontWeight::SEMIBOLD;
    let prefix = format!("{}: ", row.author);
    let mut runs = vec![TextRun {
        len: prefix.len(),
        font: strong.clone(),
        color: colors.text_muted,
        background_color: None,
        underline: None,
        strikethrough: None,
    }];
    runs.extend(row.pieces.iter().map(|piece| TextRun {
        len: piece.range.len(),
        font: if piece.highlighted {
            strong.clone()
        } else {
            face.clone()
        },
        color: colors.text,
        background_color: piece.highlighted.then_some(colors.highlight),
        underline: None,
        strikethrough: None,
    }));
    let text: SharedString = format!("{prefix}{}", row.snippet).into();
    let runs = emoji_font::with_emoji_font(&text, runs);
    StyledText::new(text).with_runs(runs)
}

/// A centered note with an optional heading and button.
fn message_block(
    theme: &Theme,
    selector: &'static str,
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
        .gap_2()
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

/// The first page loading: rows the shape of results, so nothing moves when they arrive.
fn skeleton(theme: &Theme) -> AnyElement {
    let colors = theme.colors;
    let bar = |width: f32, height: f32| {
        div()
            .h(px(height))
            .w(px(width))
            .max_w_full()
            .rounded(theme.radii.small)
            .bg(colors.border)
    };
    div()
        .id("search-loading")
        .debug_selector(|| "search-loading".into())
        .role(Role::Status)
        .aria_label("Searching")
        .flex_1()
        .min_h_0()
        .overflow_hidden()
        .children((0..SKELETON_ROWS).map(|index| {
            let wide = 260.0 + (index % 3) as f32 * 70.0;
            div().h(px(ROW_HEIGHT)).w_full().px_3().py_0p5().child(
                div()
                    .h_full()
                    .px_3()
                    .py_2()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(bar(120.0, 12.0))
                    .child(bar(wide + 120.0, 12.0))
                    .child(bar(wide, 12.0)),
            )
        }))
        .into_any_element()
}

/// The rows for the core's current results.
fn rows_of(core: &ClientCore, view: &SearchView) -> Vec<Row> {
    view.hits
        .iter()
        .map(|hit| {
            let ranges: Vec<(u32, u32)> = hit
                .snippet
                .highlights
                .iter()
                .map(|range| (range.start, range.end))
                .collect();
            Row {
                place: core.search_hit_title(hit),
                joined: core.search_hit_joined(hit),
                author: core.display_name(hit.message.author_id).to_owned(),
                created_at: hit.message.created_at,
                reply: hit.message.thread_root.is_some(),
                file: matches!(hit.kind, SearchHitKind::Attachment { .. }),
                snippet: hit.snippet.text.clone().into(),
                pieces: search_text::highlight_pieces(&hit.snippet.text, &ranges),
            }
        })
        .collect()
}

impl Focusable for SearchResultsView {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.field.read(cx).focus_handle(cx)
    }
}

/// `value.pipe(f)`: calls `f` with `value`.
trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

impl Render for SearchResultsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let header = self.header(&theme, window, cx);
        let body = self.body(&theme, window, cx);
        // Close comes last in the tab order, so it is a child of the whole view, placed over the
        // header's right end.
        let close = self.close_button(&theme, cx);
        div()
            .id("search-view")
            .relative()
            .debug_selector(|| "search-view".into())
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.colors.surface)
            .on_key_down(cx.listener(Self::on_key_down))
            .child(header)
            .child(body)
            .child(close)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_have_a_fixed_height_so_loading_and_loaded_lists_match() {
        const { assert!(ROW_HEIGHT >= 80.0 && RECENT_HEIGHT < ROW_HEIGHT) };
        assert_eq!(DEBOUNCE, Duration::from_millis(250));
    }
}
