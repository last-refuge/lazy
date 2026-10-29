//! The emoji picker: a search field, group tabs, a scrollable grid, a preview line, and a skin
//! tone selector.
//!
//! It is a plain entity that knows nothing about reactions or the composer. A host creates it with
//! the person's saved skin tone and recently used emoji, shows it as a popover, and reacts to its
//! [`EmojiPickerEvent`]s: a chosen emoji, a changed skin tone, or a request to close.
//!
//! # Keyboard
//!
//! Tab and Shift+Tab move between four stops: the search field, the group tabs, the grid, and the
//! skin tones. In the grid the arrows move in two dimensions, Home and End go to the first and last
//! emoji, PageUp and PageDown to the start of the previous or next group, and Enter or Space
//! chooses. On the tabs Left and Right move and jump to the group; on the skin tones Left and Right
//! change the tone. Down from the search field goes to the grid and Up from the top row comes back.
//! Enter in the search field chooses the highlighted result (the first, to begin with). Escape
//! closes, except while an input method is composing, when it belongs to the composition.
//!
//! # Cost
//!
//! The grid is built as data once (about 1,900 emoji in about 215 rows of nine) and only the rows
//! on screen become elements, through a uniform list, so opening costs the same however many emoji
//! there are.

use gpui::{
    AnyElement, App, Context, ElementId, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    IntoElement, KeyBinding, KeyDownEvent, Render, Role, ScrollStrategy, SharedString,
    Subscription, UniformListScrollHandle, Window, actions, div, prelude::*, px, uniform_list,
};
use lazy_domain::emoji::{self, Emoji, SkinTone};
use lazy_domain::{EmojiId, ReactionEmoji};

use crate::app_model::AppModel;
use crate::emoji_font;
use crate::emoji_grid::{COLUMNS, Grid, Nav, Pick, Row, SectionKind};
use crate::emoji_images;
use crate::image_store::ImageStore;
use crate::states;
use crate::text_input::{TextInput, TextInputEvent, TextInputOptions};
use crate::theme::Theme;

/// Key context of the picker.
pub const PICKER_CONTEXT: &str = "LazyEmojiPicker";

actions!(lazy_emoji_picker, [PickerNext, PickerPrevious]);

/// Registers the picker's Tab and Shift+Tab, which the shell would otherwise take to move focus
/// out of the popover. Called once from [`crate::init`].
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", PickerNext, Some(PICKER_CONTEXT)),
        KeyBinding::new("shift-tab", PickerPrevious, Some(PICKER_CONTEXT)),
    ]);
}

const WIDTH: f32 = 340.0;
const HEIGHT: f32 = 420.0;
/// Height of every grid row, headings included, so the list can be uniform.
const ROW_HEIGHT: f32 = 36.0;
const CELL_WIDTH: f32 = 36.0;
/// Most results a search shows.
const MAX_RESULTS: usize = 200;

/// What the picker reports to its host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmojiPickerEvent {
    /// The person chose this emoji.
    Chosen(ReactionEmoji),
    /// The person picked a skin tone (0 to 5) to remember.
    SkinToneChosen(u8),
    /// The picker should be closed without a choice.
    Dismiss,
    /// An admin asked, from the empty Custom section, to add a custom emoji: the host closes the
    /// picker and opens the custom emoji dialog.
    AddCustomEmoji,
}

/// The part of the person's saved preferences the picker needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PickerPrefs {
    pub skin_tone: u8,
    /// Most recent first.
    pub recent: Vec<ReactionEmoji>,
}

/// Which of the picker's four tab stops has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Search,
    Tabs,
    Grid,
    Tones,
}

const STOPS: [Stop; 4] = [Stop::Search, Stop::Tabs, Stop::Grid, Stop::Tones];

pub struct EmojiPicker {
    prefs: PickerPrefs,
    search: Entity<TextInput>,
    /// The browsing grid, rebuilt when the skin tone changes.
    browse: Grid,
    /// What is on screen: the browsing grid, or a search's results.
    grid: Grid,
    searching: bool,
    selected: Option<usize>,
    hovered: Option<usize>,
    scroll: UniformListScrollHandle,
    tabs_focus: FocusHandle,
    grid_focus: FocusHandle,
    tones_focus: FocusHandle,
    /// Where the workspace's custom emoji come from.
    model: Entity<AppModel>,
    _subscription: Subscription,
    /// Repaints when a custom emoji's picture finishes decoding.
    _images: Subscription,
}

impl EventEmitter<EmojiPickerEvent> for EmojiPicker {}

impl Focusable for EmojiPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search.read(cx).focus_handle(cx)
    }
}

/// The workspace's live custom emoji, by name.
fn live_custom_ids(model: &Entity<AppModel>, cx: &App) -> Vec<EmojiId> {
    model
        .read(cx)
        .core()
        .live_custom_emoji()
        .map(|emoji| emoji.id)
        .collect()
}

/// The recently used emoji as cells: standard ones, and custom ones by identity.
fn recent_picks(prefs: &PickerPrefs) -> Vec<Pick> {
    prefs
        .recent
        .iter()
        .map(|emoji| match emoji {
            ReactionEmoji::Unicode(emoji) => Pick::Standard(emoji),
            ReactionEmoji::Custom(id) => Pick::Custom(*id),
        })
        .collect()
}

impl EmojiPicker {
    /// Opens with the search field focused.
    pub fn new(
        prefs: PickerPrefs,
        model: Entity<AppModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            TextInput::new(
                TextInputOptions::new("Search emoji")
                    .accessible_label("Search emoji")
                    .single_line(true)
                    .pass_through_navigation(true)
                    .max_bytes(64),
                cx,
            )
        });
        let subscription = cx.subscribe_in(
            &search,
            window,
            |this, _search, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Changed => this.search_changed(cx),
                TextInputEvent::Submit => this.choose_selected(cx),
                TextInputEvent::Tab { backwards } => this.cycle(*backwards, window, cx),
                TextInputEvent::UpWhenEmpty
                | TextInputEvent::PasteAttachments
                | TextInputEvent::Suggestion(_) => {}
            },
        );
        let custom = live_custom_ids(&model, cx);
        let browse = Grid::browse(
            &recent_picks(&prefs),
            &custom,
            emoji::skin_tone_from_index(prefs.skin_tone),
            COLUMNS,
        );
        let images = cx.observe(&ImageStore::global(cx), |_, _, cx| cx.notify());
        let picker = Self {
            prefs,
            search,
            grid: browse.clone(),
            browse,
            searching: false,
            selected: None,
            hovered: None,
            scroll: UniformListScrollHandle::new(),
            tabs_focus: cx.focus_handle().tab_stop(true),
            grid_focus: cx.focus_handle().tab_stop(true),
            tones_focus: cx.focus_handle().tab_stop(true),
            model,
            _subscription: subscription,
            _images: images,
        };
        picker.focus_search(window, cx);
        picker
    }

    fn focus_search(&self, window: &mut Window, cx: &mut App) {
        let handle = self.search.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// Takes a skin tone chosen elsewhere (another window, say) while the picker is open.
    pub fn set_skin_tone(&mut self, tone: u8, cx: &mut Context<Self>) {
        if self.prefs.skin_tone == tone {
            return;
        }
        self.prefs.skin_tone = tone;
        self.rebuild(cx);
    }

    pub fn skin_tone(&self) -> u8 {
        self.prefs.skin_tone
    }

    /// The search field, for hosts and tests that need to reach it.
    pub fn search_input(&self) -> &Entity<TextInput> {
        &self.search
    }

    pub fn query(&self, cx: &App) -> String {
        self.search.read(cx).text().to_owned()
    }

    pub fn selected_emoji(&self) -> Option<&'static Emoji> {
        self.selected_pick().and_then(Pick::emoji)
    }

    /// What the selection is on: a standard or a custom emoji.
    pub fn selected_pick(&self) -> Option<Pick> {
        self.selected
            .and_then(|ix| self.grid.items().get(ix).copied())
    }

    fn tone(&self) -> SkinTone {
        emoji::skin_tone_from_index(self.prefs.skin_tone)
    }

    /// Rebuilds the browsing grid (for a new skin tone) and the search results, keeping the query.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let custom = live_custom_ids(&self.model, cx);
        self.browse = Grid::browse(&recent_picks(&self.prefs), &custom, self.tone(), COLUMNS);
        self.search_changed(cx);
    }

    fn search_changed(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).text().trim().to_owned();
        self.searching = !query.is_empty();
        self.hovered = None;
        if self.searching {
            let tone = self.tone();
            // The workspace's own emoji come first: a name typed is more likely theirs.
            let mut found: Vec<Pick> = self
                .model
                .read(cx)
                .core()
                .custom_emoji_search(&query, MAX_RESULTS)
                .into_iter()
                .map(|emoji| Pick::Custom(emoji.id))
                .collect();
            found.extend(
                emoji::search(&query, MAX_RESULTS)
                    .into_iter()
                    .map(|e| Pick::Standard(crate::emoji_font::drawable_tone(e, tone))),
            );
            found.truncate(MAX_RESULTS);
            self.grid = Grid::results(found, COLUMNS);
            // The first result is the one Enter picks.
            self.selected = (!self.grid.is_empty()).then_some(0);
        } else {
            self.grid = self.browse.clone();
            self.selected = None;
        }
        self.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
        cx.notify();
    }

    // ----- Choosing -----

    fn choose(&mut self, item: usize, cx: &mut Context<Self>) {
        let choice = match self.grid.items().get(item).copied() {
            Some(Pick::Standard(emoji)) => ReactionEmoji::parse(emoji.as_str()).ok(),
            Some(Pick::Custom(id)) => Some(ReactionEmoji::Custom(id)),
            None => None,
        };
        if let Some(choice) = choice {
            cx.emit(EmojiPickerEvent::Chosen(choice));
        }
    }

    fn choose_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(item) = self.selected {
            self.choose(item, cx);
        }
    }

    fn choose_tone(&mut self, tone: u8, cx: &mut Context<Self>) {
        if tone > 5 || tone == self.prefs.skin_tone {
            return;
        }
        self.prefs.skin_tone = tone;
        self.rebuild(cx);
        cx.emit(EmojiPickerEvent::SkinToneChosen(tone));
    }

    // ----- Moving around -----

    fn select(&mut self, item: usize, cx: &mut Context<Self>) {
        self.selected = Some(item);
        self.hovered = None;
        let row = self.grid.row_of(item);
        self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn jump_to_section(&mut self, section: usize, cx: &mut Context<Self>) {
        if section >= self.grid.sections().len() {
            return;
        }
        let row = self.grid.scroll_row(section);
        self.scroll.scroll_to_item_strict(row, ScrollStrategy::Top);
        cx.notify();
    }

    /// The section the grid is scrolled to, for highlighting its tab.
    fn current_section(&self) -> usize {
        let top_row = {
            let state = self.scroll.0.borrow();
            match state.deferred_scroll_to_item {
                // A jump that the next paint has not applied yet.
                Some(pending) => pending.item_index,
                None => {
                    let offset = state.base_handle.offset();
                    ((-offset.y).max(px(0.)) / px(ROW_HEIGHT)) as usize
                }
            }
        };
        let sections = self.grid.sections();
        sections
            .partition_point(|s| s.header_row.unwrap_or(s.first_row) <= top_row)
            .saturating_sub(1)
    }

    fn focus_stop(&self, stop: Stop, window: &mut Window, cx: &mut Context<Self>) {
        match stop {
            Stop::Search => self.focus_search(window, cx),
            Stop::Tabs => window.focus(&self.tabs_focus, cx),
            Stop::Grid => window.focus(&self.grid_focus, cx),
            Stop::Tones => window.focus(&self.tones_focus, cx),
        }
    }

    fn current_stop(&self, window: &Window, cx: &App) -> Stop {
        if self.tabs_focus.is_focused(window) {
            Stop::Tabs
        } else if self.grid_focus.is_focused(window) {
            Stop::Grid
        } else if self.tones_focus.is_focused(window) {
            Stop::Tones
        } else {
            let _ = cx;
            Stop::Search
        }
    }

    /// Tab and Shift+Tab: the next stop, skipping the tabs while a search is showing results.
    fn cycle(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        let usable = |stop: Stop| !(self.searching && stop == Stop::Tabs);
        let mut at = STOPS
            .iter()
            .position(|s| *s == self.current_stop(window, cx))
            .unwrap_or(0);
        for _ in 0..STOPS.len() {
            at = if backwards {
                (at + STOPS.len() - 1) % STOPS.len()
            } else {
                (at + 1) % STOPS.len()
            };
            if usable(STOPS[at]) {
                break;
            }
        }
        self.focus_stop(STOPS[at], window, cx);
        if STOPS[at] == Stop::Grid && self.selected.is_none() && !self.grid.is_empty() {
            self.select(0, cx);
        }
    }

    fn move_in_grid(&mut self, nav: Nav, window: &mut Window, cx: &mut Context<Self>) {
        if self.grid.is_empty() {
            return;
        }
        match self.selected {
            None => self.select(0, cx),
            Some(from) => {
                // Up from the first row goes back to the search field.
                if nav == Nav::Up
                    && self.grid.row_of(from)
                        <= self
                            .grid
                            .rows()
                            .iter()
                            .position(|r| matches!(r, Row::Cells { .. }))
                            .unwrap_or(0)
                {
                    self.focus_stop(Stop::Search, window, cx);
                    return;
                }
                let to = self.grid.step(from, nav);
                self.select(to, cx);
            }
        }
    }

    fn move_tab(&mut self, forwards: bool, cx: &mut Context<Self>) {
        let count = self.grid.sections().len();
        if count == 0 {
            return;
        }
        let at = self.current_section();
        let next = if forwards {
            (at + 1).min(count - 1)
        } else {
            at.saturating_sub(1)
        };
        self.jump_to_section(next, cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let plain = !event.keystroke.modifiers.modified();
        let stop = self.current_stop(window, cx);
        let handled = match (key, plain, stop) {
            ("escape", true, _) => {
                // An input method composing in the search field owns Escape.
                if self.search.read(cx).is_composing() {
                    return;
                }
                cx.emit(EmojiPickerEvent::Dismiss);
                true
            }
            ("down", true, Stop::Search) => {
                self.focus_stop(Stop::Grid, window, cx);
                if self.selected.is_none() && !self.grid.is_empty() {
                    self.select(0, cx);
                }
                true
            }
            ("down", true, Stop::Tabs) => {
                self.focus_stop(Stop::Grid, window, cx);
                let section = self.current_section();
                let first = self.grid.sections().get(section).map(|s| s.first_item);
                if let Some(first) = first {
                    self.select(first, cx);
                }
                true
            }
            ("left", true, Stop::Tabs) => {
                self.move_tab(false, cx);
                true
            }
            ("right", true, Stop::Tabs) => {
                self.move_tab(true, cx);
                true
            }
            ("left", true, Stop::Tones) => {
                self.choose_tone(self.prefs.skin_tone.saturating_sub(1), cx);
                true
            }
            ("right", true, Stop::Tones) => {
                self.choose_tone((self.prefs.skin_tone + 1).min(5), cx);
                true
            }
            (_, true, Stop::Grid) => {
                let nav = match key {
                    "left" => Some(Nav::Left),
                    "right" => Some(Nav::Right),
                    "up" => Some(Nav::Up),
                    "down" => Some(Nav::Down),
                    "home" => Some(Nav::Home),
                    "end" => Some(Nav::End),
                    "pageup" => Some(Nav::PageUp),
                    "pagedown" => Some(Nav::PageDown),
                    _ => None,
                };
                match (nav, key) {
                    (Some(nav), _) => {
                        self.move_in_grid(nav, window, cx);
                        true
                    }
                    (None, "enter" | "space") => {
                        self.choose_selected(cx);
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        };
        if handled {
            cx.stop_propagation();
        }
    }

    // ----- Drawing -----

    fn glyph_button(
        &self,
        id: ElementId,
        selector: String,
        glyph: &str,
        size: f32,
        label: SharedString,
    ) -> gpui::Stateful<gpui::Div> {
        self.cell_button(
            id,
            selector,
            label,
            div()
                .text_size(px(size))
                .child(emoji_font::glyphs(glyph.to_owned())),
        )
    }

    fn cell_button(
        &self,
        id: ElementId,
        selector: String,
        label: SharedString,
        content: impl IntoElement,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .debug_selector(move || selector)
            .role(Role::Button)
            .aria_label(label)
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .child(content)
    }

    /// A custom emoji's picture in a `size` square, or a muted stand-in until it has one.
    fn custom_face(&self, id: EmojiId, size: f32, theme: &Theme, cx: &App) -> AnyElement {
        let picture = emoji_images::picture(ImageStore::global(cx).read(cx), id);
        match picture {
            Some(picture) => emoji_images::square_picture(&picture.image, size)
                .debug_selector(|| "emoji-custom-picture".into())
                .into_any_element(),
            None => div()
                .size(px(size))
                .flex()
                .items_center()
                .justify_center()
                .text_size(theme.type_scale.small)
                .text_color(theme.colors.text_faint)
                .child("…")
                .into_any_element(),
        }
    }

    fn render_rows(
        &mut self,
        range: std::ops::Range<usize>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let colors = theme.colors;
        let mut out = Vec::with_capacity(range.len());
        // The custom emoji on screen: their pictures are asked for after this frame.
        let mut visible_custom: Vec<EmojiId> = Vec::new();
        let can_add = self.model.read(cx).core().can_manage_emoji();
        for row_ix in range {
            let Some(row) = self.grid.rows().get(row_ix).copied() else {
                continue;
            };
            out.push(match row {
                Row::Header(section) => {
                    let section = &self.grid.sections()[section];
                    div()
                        .h(px(ROW_HEIGHT))
                        .px_1()
                        .flex()
                        .items_end()
                        .pb_1()
                        .text_size(theme.type_scale.small)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.text_muted)
                        .child(section.title)
                        .into_any_element()
                }
                Row::Note(_) => {
                    div()
                        .h(px(ROW_HEIGHT))
                        .px_1()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .debug_selector(|| "emoji-custom-empty".into())
                                .text_size(theme.type_scale.small)
                                .text_color(colors.text_muted)
                                .child(states::NO_CUSTOM_EMOJI_PICKER),
                        )
                        .children(can_add.then(|| {
                            div()
                                .id("emoji-add-custom")
                                .debug_selector(|| "emoji-add-custom".into())
                                .role(Role::Button)
                                .aria_label("Add custom emoji")
                                .cursor_pointer()
                                .px_2()
                                .py_0p5()
                                .rounded(theme.radii.small)
                                .border_1()
                                .border_color(colors.border_strong)
                                .text_size(theme.type_scale.small)
                                .text_color(colors.text)
                                .hover(|style| style.bg(colors.surface_hover))
                                .on_click(cx.listener(|_, _, _, cx| {
                                    cx.emit(EmojiPickerEvent::AddCustomEmoji)
                                }))
                                .child("Add custom emoji")
                        }))
                        .into_any_element()
                }
                Row::Cells { first_item, len } => {
                    let mut cells = div().h(px(ROW_HEIGHT)).flex();
                    for item in first_item..first_item + len {
                        let pick = self.grid.items()[item];
                        let selected = self.selected == Some(item);
                        let button = match pick {
                            Pick::Standard(emoji) => self.glyph_button(
                                ElementId::NamedInteger("emoji-cell".into(), item as u64),
                                format!("emoji-cell-{item}"),
                                emoji.as_str(),
                                22.0,
                                emoji.name().to_owned().into(),
                            ),
                            Pick::Custom(id) => {
                                visible_custom.push(id);
                                let name =
                                    emoji_images::label(self.model.read(cx).core(), id, None);
                                self.cell_button(
                                    ElementId::NamedInteger("emoji-cell".into(), item as u64),
                                    format!("emoji-cell-{item}"),
                                    format!("{name} emoji").into(),
                                    self.custom_face(id, 24.0, theme, cx),
                                )
                            }
                        };
                        cells = cells.child(
                            button
                                .w(px(CELL_WIDTH))
                                .h(px(ROW_HEIGHT))
                                .rounded(theme.radii.small)
                                .when(selected, |cell| cell.bg(colors.selection))
                                .hover(|style| style.bg(colors.surface_hover))
                                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                                    if this.hovered != Some(item) {
                                        this.hovered = Some(item);
                                        cx.notify();
                                    }
                                }))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.grid_focus.focus(window, cx);
                                    this.selected = Some(item);
                                    this.choose(item, cx);
                                })),
                        );
                    }
                    cells.into_any_element()
                }
            });
        }
        emoji_images::request_later(&self.model, visible_custom, cx);
        out
    }

    fn preview(&self, theme: &Theme, cx: &App) -> AnyElement {
        let colors = theme.colors;
        let shown = self
            .hovered
            .or(self.selected)
            .and_then(|ix| self.grid.items().get(ix).copied());
        if let Some(Pick::Custom(id)) = shown {
            // A custom emoji: its picture, `:name:`, and who added it.
            let core = self.model.read(cx).core();
            let by = core
                .custom_emoji_by_id(id)
                .map(|emoji| core.display_name(emoji.created_by).to_owned())
                .map(|name| format!("by {name}"))
                .unwrap_or_else(|| "Custom emoji".to_owned());
            return div()
                .debug_selector(|| "emoji-preview".into())
                .flex()
                .items_center()
                .gap_2()
                .min_w_0()
                .child(self.custom_face(id, 28.0, theme, cx))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .child(
                            div()
                                .debug_selector(|| "emoji-preview-name".into())
                                .truncate()
                                .text_size(theme.type_scale.body)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.text)
                                .child(emoji_images::label(core, id, None)),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(theme.type_scale.small)
                                .text_color(colors.text_muted)
                                .child(by),
                        ),
                )
                .into_any_element();
        }
        match shown.and_then(Pick::emoji) {
            Some(emoji) => {
                let shortcode = emoji
                    .shortcode()
                    .map(|s| format!(":{s}:"))
                    .unwrap_or_default();
                div()
                    .debug_selector(|| "emoji-preview".into())
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(28.))
                            .child(emoji_font::glyphs(emoji.as_str().to_owned())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .child(
                                div()
                                    .max_h(px(36.))
                                    .overflow_hidden()
                                    .text_size(theme.type_scale.body)
                                    .line_height(px(18.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(colors.text)
                                    .child(emoji.name().to_owned()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.text_muted)
                                    .child(shortcode),
                            ),
                    )
                    .into_any_element()
            }
            None => div()
                .text_size(theme.type_scale.small)
                .text_color(colors.text_faint)
                .child(if self.searching {
                    "No emoji match."
                } else {
                    "Pick an emoji"
                })
                .into_any_element(),
        }
    }

    fn tones(&self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let focused = self.tones_focus.is_focused(window);
        let wave = emoji::lookup("👋").expect("a waving hand");
        let mut row = div()
            .id("emoji-tones")
            .debug_selector(|| "emoji-tones".into())
            .track_focus(&self.tones_focus)
            .role(Role::RadioGroup)
            .aria_label("Skin tone")
            .flex()
            .gap_0p5()
            .p_0p5()
            .rounded(theme.radii.small)
            .border_1()
            .border_color(if focused {
                colors.focus_ring
            } else {
                gpui::transparent_black()
            });
        for tone in 0..=5u8 {
            let glyph =
                crate::emoji_font::drawable_tone(wave, emoji::skin_tone_from_index(tone)).as_str();
            let chosen = tone == self.prefs.skin_tone;
            row = row.child(
                self.glyph_button(
                    ElementId::NamedInteger("emoji-tone".into(), u64::from(tone)),
                    format!("emoji-tone-{tone}"),
                    glyph,
                    16.0,
                    format!("Skin tone {}", tone + 1).into(),
                )
                .size(px(26.))
                .rounded(theme.radii.small)
                .when(chosen, |button| {
                    button
                        .border_1()
                        .border_color(colors.accent)
                        .bg(colors.selection)
                })
                .hover(|style| style.bg(colors.surface_hover))
                .on_click(cx.listener(move |this, _, _, cx| this.choose_tone(tone, cx))),
            );
        }
        row.into_any_element()
    }

    fn tabs(&self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let current = self.current_section();
        let focused = self.tabs_focus.is_focused(window);
        let mut row = div()
            .id("emoji-tabs")
            .debug_selector(|| "emoji-tabs".into())
            .track_focus(&self.tabs_focus)
            .role(Role::TabList)
            .aria_label("Emoji groups")
            .flex()
            .px_1()
            .border_b_1()
            .border_color(if focused {
                colors.focus_ring
            } else {
                colors.border
            });
        for (ix, section) in self.grid.sections().iter().enumerate() {
            let active = ix == current;
            row = row.child(
                self.glyph_button(
                    ElementId::NamedInteger("emoji-tab".into(), ix as u64),
                    format!("emoji-tab-{ix}"),
                    section.icon,
                    18.0,
                    section.title.into(),
                )
                .h(px(34.))
                .flex_1()
                .border_b_2()
                .border_color(if active {
                    colors.accent
                } else {
                    gpui::transparent_black()
                })
                .when(!active, |tab| tab.opacity(0.7))
                .hover(|style| style.bg(colors.surface_hover))
                .on_click(cx.listener(move |this, _, _, cx| this.jump_to_section(ix, cx))),
            );
        }
        row.into_any_element()
    }
}

impl Render for EmojiPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        let row_count = self.grid.rows().len();
        let grid_focused = self.grid_focus.is_focused(window);

        let list = uniform_list(
            "emoji-grid",
            row_count,
            cx.processor(
                move |this,
                      range: std::ops::Range<usize>,
                      _window: &mut Window,
                      cx: &mut Context<Self>| {
                    let theme = *Theme::get(cx);
                    this.render_rows(range, &theme, cx)
                },
            ),
        )
        .track_scroll(&self.scroll)
        .size_full();

        div()
            .id("emoji-picker")
            .debug_selector(|| "emoji-picker".into())
            .key_context(PICKER_CONTEXT)
            .role(Role::Dialog)
            .aria_label("Emoji picker")
            .occlude()
            .on_action(
                cx.listener(|this, _: &PickerNext, window, cx| this.cycle(false, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &PickerPrevious, window, cx| this.cycle(true, window, cx)),
            )
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(EmojiPickerEvent::Dismiss)))
            .w(px(WIDTH))
            .h(px(HEIGHT))
            .flex()
            .flex_col()
            .rounded(theme.radii.large)
            .bg(colors.surface_raised)
            .border_1()
            .border_color(colors.border_strong)
            .shadow_lg()
            .overflow_hidden()
            .child(
                div().px_3().pt_3().pb_2().child(
                    div()
                        .px_3()
                        .py_1p5()
                        .rounded(theme.radii.medium)
                        .bg(colors.surface)
                        .border_1()
                        .border_color(if self.search.read(cx).is_focused(window) {
                            colors.focus_ring
                        } else {
                            colors.border_strong
                        })
                        .child(self.search.clone()),
                ),
            )
            .when(!self.searching, |picker| {
                picker.child(self.tabs(&theme, window, cx))
            })
            .child(
                div()
                    .id("emoji-grid-area")
                    .debug_selector(|| "emoji-grid".into())
                    .track_focus(&self.grid_focus)
                    .role(Role::Grid)
                    .aria_label("Emoji")
                    .flex_1()
                    .min_h_0()
                    .px_2()
                    .border_1()
                    .border_color(if grid_focused {
                        colors.focus_ring
                    } else {
                        gpui::transparent_black()
                    })
                    .child(if row_count == 0 {
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(theme.type_scale.body)
                            .text_color(colors.text_muted)
                            .child("No emoji match that search.")
                            .into_any_element()
                    } else {
                        list.into_any_element()
                    }),
            )
            .child(
                div()
                    .h(px(52.))
                    .flex_none()
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .border_t_1()
                    .border_color(colors.border)
                    .child(div().flex_1().min_w_0().child(self.preview(&theme, cx)))
                    .child(self.tones(&theme, window, cx)),
            )
    }
}

/// The emoji in a section header's kind, for tests and hosts that want to name it.
pub fn section_kind_name(kind: SectionKind) -> &'static str {
    match kind {
        SectionKind::Recent => "recent",
        SectionKind::Custom => "custom",
        SectionKind::Group(_) => "group",
        SectionKind::Results => "results",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_kinds_have_names() {
        assert_eq!(section_kind_name(SectionKind::Recent), "recent");
        assert_eq!(section_kind_name(SectionKind::Results), "results");
    }
}
