//! The quick switcher (Ctrl+K): jump to a conversation, a person, or a place by typing a few of
//! its letters.
//!
//! The core ranks and filters (`quick_switcher_results`); this view draws what it returns, keeps
//! the keyboard, and reports the choice. With no text it lists where the person has been; with
//! text the matched characters of each title are emphasised.
//!
//! # Keyboard
//!
//! A query that starts with `>` lists commands instead of places (fuzzy matched, only those that
//! can run, each with its shortcut); Enter runs the highlighted one.
//!
//! The field has the keyboard when it opens. Up and Down move the highlighted row (wrapping),
//! Enter opens it, Escape closes the switcher, and Ctrl+K (Cmd+K on a Mac) closes it too. Tab
//! stays inside: the field is the one stop. The mouse highlights a row as it passes over it and a
//! click opens it. Closing returns the keyboard to where it was; choosing a conversation puts it
//! in that conversation's composer (the shell does that).

use gpui::{
    AnyElement, Context, ElementId, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, KeyBinding, ParentElement, Render, Role, ScrollHandle, Styled,
    Subscription, Window, actions, div, prelude::*, px, transparent_black,
};
use lazy_client_core::{QuickSwitcherItem, QuickSwitcherTarget};
use lazy_domain::ConversationKind;

use super::any_composing;
use crate::app_model::{AppEvent, AppModel};
use crate::commands::{self, CommandId};
use crate::emoji_font;
use crate::emphasis;
use crate::modal::{
    DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, palette_overlay,
};
use crate::sidebar::badge_text;
use crate::states;
use crate::text_input::actions::{MoveDown, MoveUp};
use crate::text_input::{TextInput, TextInputEvent, TextInputOptions};
use crate::theme::Theme;

/// Key context of the open switcher, for the key that closes it.
const CONTEXT: &str = "LazyQuickSwitcher";

actions!(lazy_quick_switcher, [ToggleQuickSwitcher]);

/// The row height, so a row can be found from the pointer and the list never re-measures.
const ROW_HEIGHT: f32 = 44.0;

/// Registers the switcher's keys: Ctrl+K opens it from anywhere in the shell (see
/// [`crate::keyboard`]) and closes it from inside. Called once from [`crate::init`].
pub fn bind_keys(cx: &mut gpui::App) {
    cx.bind_keys([KeyBinding::new(
        if cfg!(target_os = "macos") {
            "cmd-k"
        } else {
            "ctrl-k"
        },
        ToggleQuickSwitcher,
        Some(CONTEXT),
    )]);
}

/// What the switcher tells the shell besides "close me".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickSwitcherChoice(pub QuickSwitcherTarget);

/// A command chosen from the `>` list: the shell runs it once the switcher has closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuickSwitcherRun(pub CommandId);

pub struct QuickSwitcherDialog {
    model: Entity<AppModel>,
    trap: ModalTrap,
    input: Entity<TextInput>,
    items: Vec<QuickSwitcherItem>,
    /// With a `>` query: the commands that can run, instead of `items`.
    commands: Vec<commands::Row>,
    highlighted: usize,
    scroll: ScrollHandle,
    /// What the field held when `items` was computed.
    query: String,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for QuickSwitcherDialog {}
impl EventEmitter<QuickSwitcherChoice> for QuickSwitcherDialog {}
impl EventEmitter<QuickSwitcherRun> for QuickSwitcherDialog {}

impl QuickSwitcherDialog {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::with_query(model, "", window, cx)
    }

    /// Opens with `query` typed in the field (`>` for the commands).
    pub fn with_query(
        model: Entity<AppModel>,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextInput::new(
                TextInputOptions::new(states::QUICK_PLACEHOLDER)
                    .accessible_label("Quick switcher")
                    .single_line(true)
                    .pass_through_navigation(true)
                    .max_bytes(120),
                cx,
            )
        });
        let mut subscriptions = Vec::new();
        subscriptions.push(
            cx.observe(&crate::image_store::ImageStore::global(cx), |_, _, cx| {
                cx.notify()
            }),
        );
        subscriptions.push(cx.subscribe_in(
            &input,
            window,
            |this, _input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Changed => {
                    this.highlighted = 0;
                    this.refresh(cx);
                }
                TextInputEvent::Submit => this.choose(this.highlighted, window, cx),
                TextInputEvent::Tab { backwards } => this.trap.tab(*backwards, window, cx),
                TextInputEvent::UpWhenEmpty
                | TextInputEvent::PasteAttachments
                | TextInputEvent::Suggestion(_) => {}
            },
        ));
        // Unread counts and the recent list move while it is open.
        subscriptions.push(cx.subscribe_in(
            &model,
            window,
            |this, _model, _event: &AppEvent, _window, cx| this.refresh(cx),
        ));
        let mut trap = ModalTrap::new(cx);
        let initial = input.focus_handle(cx);
        trap.open(&initial, window, cx);
        if !query.is_empty() {
            input.update(cx, |input, cx| input.set_text(query, cx));
        }
        let mut dialog = Self {
            model,
            trap,
            input,
            items: Vec::new(),
            commands: Vec::new(),
            highlighted: 0,
            scroll: ScrollHandle::new(),
            query: String::new(),
            _subscriptions: subscriptions,
        };
        dialog.refresh(cx);
        // The first recent is where the person already is: Enter at once goes back to the one
        // before it.
        let current = dialog.model.read(cx).core().selected_conversation();
        if dialog.items.len() > 1
            && matches!(
                &dialog.items[0].target,
                QuickSwitcherTarget::Conversation(id) if Some(*id) == current
            )
        {
            dialog.highlighted = 1;
        }
        dialog
    }

    /// What is on screen, for tests.
    pub fn rows(&self) -> &[QuickSwitcherItem] {
        &self.items
    }

    /// The commands listed while the query starts with `>`, for tests.
    pub fn command_rows(&self) -> &[commands::Row] {
        &self.commands
    }

    /// Whether the query asks for commands.
    fn command_mode(&self) -> bool {
        self.query.trim_start().starts_with('>')
    }

    /// The highlighted row, for tests.
    pub fn highlighted(&self) -> usize {
        self.highlighted
    }

    /// The field's focus handle, for tests.
    pub fn field_focus(&self, cx: &gpui::App) -> FocusHandle {
        self.input.focus_handle(cx)
    }

    /// Asks the core again; the highlight stays on the same target when it is still listed.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).text().to_owned();
        if let Some(rest) = query.trim_start().strip_prefix('>') {
            // What could run once the switcher has closed.
            let snapshot = commands::AppSnapshot::from_core(self.model.read(cx).core());
            self.commands = commands::palette_rows(rest, &snapshot);
            self.items = Vec::new();
            self.highlighted = if query == self.query {
                self.highlighted.min(self.commands.len().saturating_sub(1))
            } else {
                0
            };
            self.query = query;
            cx.notify();
            return;
        }
        self.commands = Vec::new();
        let keep = self
            .items
            .get(self.highlighted)
            .map(|item| item.target.clone())
            .filter(|_| query == self.query);
        let items = self.model.read(cx).core().quick_switcher_results(&query);
        self.highlighted = keep
            .and_then(|target| items.iter().position(|item| item.target == target))
            .unwrap_or(0);
        self.items = items;
        self.query = query;
        cx.notify();
    }

    fn move_highlight(&mut self, forwards: bool, cx: &mut Context<Self>) {
        let count = if self.command_mode() {
            self.commands.len()
        } else {
            self.items.len()
        };
        if count == 0 {
            return;
        }
        self.highlighted = if forwards {
            (self.highlighted + 1) % count
        } else {
            (self.highlighted + count - 1) % count
        };
        // The "Recent" heading is a child of the list too, when no text has been typed.
        let heading = usize::from(self.query.trim().is_empty());
        self.scroll.scroll_to_item(self.highlighted + heading);
        cx.notify();
    }

    /// Opens row `index`: closes the switcher, then tells the shell what was chosen.
    fn choose(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.command_mode() {
            let Some(row) = self.commands.get(index) else {
                return;
            };
            let id = row.id;
            self.trap.close(window, cx);
            cx.emit(QuickSwitcherRun(id));
            cx.emit(DialogEvent::Dismiss);
            return;
        }
        let Some(item) = self.items.get(index) else {
            return;
        };
        let target = item.target.clone();
        self.trap.close(window, cx);
        cx.emit(QuickSwitcherChoice(target));
        cx.emit(DialogEvent::Dismiss);
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if any_composing(&[&self.input], cx) {
            return;
        }
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }

    /// The glyph at the left of a row: `#` or a lock for a channel, an initial for a person or a
    /// conversation of people, a mark for the fixed places.
    fn leading(&self, theme: &Theme, item: &QuickSwitcherItem, cx: &gpui::App) -> AnyElement {
        let colors = theme.colors;
        let mark = |text: &'static str| {
            div()
                .size(px(24.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .text_size(theme.type_scale.body)
                .text_color(colors.text_muted)
                .child(text)
                .into_any_element()
        };
        let avatar = |seed: u128| {
            crate::avatar::Avatar::new(seed, item.title.clone(), 24.)
                .text_size(theme.type_scale.small)
                .into_any_element()
        };
        match &item.target {
            QuickSwitcherTarget::Conversation(id) => {
                let core = self.model.read(cx).core();
                match core.conversation(*id) {
                    Some(c) if c.kind == ConversationKind::Channel => {
                        if c.visibility == lazy_domain::ChannelVisibility::Private {
                            div()
                                .size(px(24.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(theme.type_scale.body)
                                .child(emoji_font::glyphs("🔒"))
                                .into_any_element()
                        } else {
                            mark("#")
                        }
                    }
                    _ => avatar(id.as_uuid().as_u128()),
                }
            }
            QuickSwitcherTarget::Person(user) => {
                crate::avatar::Avatar::new(user.as_uuid().as_u128(), item.title.clone(), 24.)
                    .text_size(theme.type_scale.small)
                    .image(crate::avatar_images::image_of(
                        self.model.read(cx).core(),
                        crate::image_store::ImageStore::global(cx).read(cx),
                        *user,
                    ))
                    .into_any_element()
            }
            QuickSwitcherTarget::Threads => mark("↳"),
            QuickSwitcherTarget::Saved => mark("★"),
            QuickSwitcherTarget::Search => mark("⌕"),
            QuickSwitcherTarget::BrowseChannels { .. } => mark("#"),
        }
    }

    /// A row of the `>` list: the command's title (the typed letters emphasised) and its shortcut.
    fn command_row(
        &self,
        theme: &Theme,
        index: usize,
        row: &commands::Row,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let highlighted = index == self.highlighted;
        let this = cx.entity().downgrade();
        let hover_to = this.clone();
        let click_to = this;
        let rest = self.query.trim_start().trim_start_matches('>').to_owned();
        let matched = lazy_domain::fuzzy::fuzzy_match(&rest, &row.title)
            .map(|found| found.positions)
            .unwrap_or_default();
        div()
            .id(ElementId::NamedInteger(
                "quick-command".into(),
                index as u64,
            ))
            .debug_selector(move || format!("quick-command-{index}"))
            .role(Role::ListItem)
            .aria_label(row.title.clone())
            .h(px(ROW_HEIGHT))
            .flex_none()
            .px_3()
            .flex()
            .items_center()
            .gap_3()
            .rounded(theme.radii.medium)
            .border_1()
            .border_color(if highlighted {
                colors.focus_ring
            } else {
                transparent_black()
            })
            .when(highlighted, |row| row.bg(colors.selection))
            .cursor_pointer()
            .on_mouse_move(move |_, _, cx| {
                hover_to
                    .update(cx, |dialog, cx| {
                        if dialog.highlighted != index {
                            dialog.highlighted = index;
                            cx.notify();
                        }
                    })
                    .ok();
            })
            .on_click(move |_, window, cx| {
                click_to
                    .update(cx, |dialog, cx| dialog.choose(index, window, cx))
                    .ok();
            })
            .child(
                div()
                    .flex_none()
                    .w(px(24.))
                    .text_color(colors.text_muted)
                    .child(">"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(theme.type_scale.body)
                    .child(emphasis::styled(
                        &row.title,
                        &matched,
                        colors.text,
                        colors.accent,
                        FontWeight::SEMIBOLD,
                    )),
            )
            .children(row.shortcut.clone().map(|shortcut| {
                div()
                    .debug_selector(move || format!("quick-command-shortcut-{index}"))
                    .flex_none()
                    .px_2()
                    .py_0p5()
                    .rounded(theme.radii.small)
                    .bg(colors.surface_hover)
                    .border_1()
                    .border_color(colors.border)
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_muted)
                    .child(shortcut)
            }))
            .into_any_element()
    }

    fn row(
        &self,
        theme: &Theme,
        index: usize,
        item: &QuickSwitcherItem,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let highlighted = index == self.highlighted;
        let this = cx.entity().downgrade();
        let hover_to = this.clone();
        let click_to = this;
        let dim = item.muted || item.archived;
        let title_color = if dim { colors.text_muted } else { colors.text };
        let title = emphasis::styled(
            &item.title,
            &item.matched,
            title_color,
            colors.accent,
            if item.unread > 0 && !item.muted {
                FontWeight::BOLD
            } else {
                FontWeight::SEMIBOLD
            },
        );
        let label = match &item.subtitle {
            Some(subtitle) => format!("{}, {subtitle}", item.title),
            None => item.title.clone(),
        };
        let unread = (item.unread > 0).then(|| {
            div()
                .debug_selector(move || format!("quick-unread-{index}"))
                .flex_none()
                .px_1p5()
                .rounded(theme.radii.small)
                .bg(if item.muted {
                    colors.surface_hover
                } else {
                    colors.accent
                })
                .text_color(if item.muted {
                    colors.text_muted
                } else {
                    colors.on_accent
                })
                .text_size(theme.type_scale.small)
                .font_weight(FontWeight::SEMIBOLD)
                .child(badge_text(item.unread).unwrap_or_default())
        });
        let mentions = (item.mentions > 0).then(|| {
            div()
                .debug_selector(move || format!("quick-mentions-{index}"))
                .flex_none()
                .px_1p5()
                .rounded(theme.radii.small)
                .bg(colors.danger)
                .text_color(colors.on_accent)
                .text_size(theme.type_scale.small)
                .font_weight(FontWeight::SEMIBOLD)
                .child(format!(
                    "@{}",
                    badge_text(item.mentions).unwrap_or_default()
                ))
        });
        let note = |text: &'static str, selector: &'static str| {
            div()
                .debug_selector(move || format!("{selector}-{index}"))
                .flex_none()
                .px_1p5()
                .rounded(theme.radii.small)
                .border_1()
                .border_color(colors.border_strong)
                .text_color(colors.text_muted)
                .text_size(theme.type_scale.small)
                .child(text)
        };
        div()
            .id(ElementId::NamedInteger("quick-row".into(), index as u64))
            .debug_selector(move || format!("quick-row-{index}"))
            .role(Role::ListItem)
            .aria_label(label)
            .h(px(ROW_HEIGHT))
            .flex_none()
            .px_3()
            .flex()
            .items_center()
            .gap_3()
            .rounded(theme.radii.medium)
            .border_1()
            .border_color(if highlighted {
                colors.focus_ring
            } else {
                transparent_black()
            })
            .when(highlighted, |row| row.bg(colors.selection))
            .cursor_pointer()
            .on_mouse_move(move |_, _, cx| {
                hover_to
                    .update(cx, |dialog, cx| {
                        if dialog.highlighted != index {
                            dialog.highlighted = index;
                            cx.notify();
                        }
                    })
                    .ok();
            })
            .on_click(move |_, window, cx| {
                click_to
                    .update(cx, |dialog, cx| dialog.choose(index, window, cx))
                    .ok();
            })
            .child(self.leading(theme, item, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .truncate()
                            .text_size(theme.type_scale.body)
                            .child(title),
                    )
                    .children(item.subtitle.clone().map(|subtitle| {
                        div()
                            .truncate()
                            .text_size(theme.type_scale.small)
                            .text_color(colors.text_muted)
                            .child(subtitle)
                    })),
            )
            .children(item.archived.then(|| note("Archived", "quick-archived")))
            .children(item.muted.then(|| note("Muted", "quick-muted")))
            .children(mentions)
            .children(unread)
            .into_any_element()
    }
}

impl Focusable for QuickSwitcherDialog {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for QuickSwitcherDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        // The pictures of the people in the list.
        crate::avatar_images::request_later(
            &self.model,
            self.items
                .iter()
                .filter_map(|item| match &item.target {
                    QuickSwitcherTarget::Person(user) => Some(*user),
                    _ => None,
                })
                .collect(),
            cx,
        );
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let command_mode = self.command_mode();
        let rows: Vec<AnyElement> = if command_mode {
            self.commands
                .clone()
                .iter()
                .enumerate()
                .map(|(index, row)| self.command_row(&theme, index, row, cx))
                .collect()
        } else {
            self.items
                .clone()
                .iter()
                .enumerate()
                .map(|(index, item)| self.row(&theme, index, item, cx))
                .collect()
        };
        let typed = !self.query.trim().is_empty();
        let rows_empty = rows.is_empty();
        let body = div()
            .flex()
            .flex_col()
            .min_h_0()
            .child(
                div()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(colors.border)
                    .text_size(theme.type_scale.title)
                    .child(self.input.clone()),
            )
            .child(
                div()
                    .id("quick-list")
                    .debug_selector(|| "quick-list".into())
                    .role(Role::List)
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .when(!typed && !rows.is_empty(), |list| {
                        list.child(
                            div()
                                .px_3()
                                .pb_1()
                                .text_size(theme.type_scale.small)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.text_muted)
                                .child("Recent"),
                        )
                    })
                    .children(rows)
                    .when(rows_empty, |list| {
                        list.child(
                            div()
                                .debug_selector(|| "quick-empty".into())
                                .px_3()
                                .py_4()
                                .text_size(theme.type_scale.body)
                                .text_color(colors.text_muted)
                                .child(if command_mode {
                                    "No command matches that.".to_owned()
                                } else if typed {
                                    states::quick_no_match(&self.query)
                                } else {
                                    "Nothing yet. Type a name to find a conversation or a person."
                                        .to_owned()
                                }),
                        )
                    }),
            )
            .child(
                div()
                    .px_4()
                    .py_2()
                    .border_t_1()
                    .border_color(colors.border)
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_faint)
                    .child(if command_mode {
                        "Up and Down move · Enter runs · Esc closes"
                    } else {
                        "Up and Down move · Enter opens · Esc closes · > for commands"
                    }),
            )
            .into_any_element();
        let close_to = this.clone();
        div()
            .absolute()
            .inset_0()
            .key_context(CONTEXT)
            .child(
                palette_overlay(&theme, 620., &self.trap, body, move |window, cx| {
                    close_to
                        .update(cx, |dialog, cx| dialog.dismiss(window, cx))
                        .ok();
                })
                .on_action(
                    cx.listener(|this, _: &DialogDismiss, window, cx| this.dismiss(window, cx)),
                )
                .on_action(
                    cx.listener(|this, _: &DialogNext, window, cx| {
                        this.trap.tab(false, window, cx)
                    }),
                )
                .on_action(
                    cx.listener(|this, _: &DialogPrev, window, cx| this.trap.tab(true, window, cx)),
                )
                .on_action(cx.listener(|this, _: &MoveDown, _, cx| this.move_highlight(true, cx)))
                .on_action(cx.listener(|this, _: &MoveUp, _, cx| this.move_highlight(false, cx))),
            )
            .on_action(
                cx.listener(|this, _: &ToggleQuickSwitcher, window, cx| this.dismiss(window, cx)),
            )
            .into_any_element()
    }
}
