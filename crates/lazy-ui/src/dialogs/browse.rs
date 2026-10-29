//! Browse channels: the directory of public channels, searchable, filterable, sortable, with
//! Join and Open, paged.
//!
//! The core owns the query, the filter, the sort and the pages (`ClientCore::browse`); this
//! dialog sends each change as an input and draws what comes back.
//!
//! # Keyboard
//!
//! The search box has the keyboard when the dialog opens. Up and Down move the highlighted row,
//! Enter joins it (or opens it, when the person is in it), Escape closes. Tab order: the search
//! box, the All / Not joined filter, the Name / Members sort, "Load more" or "Try again" when
//! there is one, and Close. Left and Right choose within a filter or sort. The Join and Open
//! buttons on the rows are for the mouse; the keyboard reaches the same through Enter.

use std::collections::HashMap;

use gpui::{
    Context, ElementId, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Render, Role, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div, prelude::*, px,
};
use lazy_client_core::{ActionState, Change, Input, LoadStatus};
use lazy_domain::ConversationId;
use lazy_protocol::{BrowsedChannel, ChannelSort};

use super::any_composing;
use crate::app_model::{AppEvent, AppModel};
use crate::form::{self, ButtonKind};
use crate::modal::{DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, dialog_overlay};
use crate::states;
use crate::text_input::actions::{MoveDown, MoveUp};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::Theme;

/// The tallest the list grows before it scrolls.
const LIST_MAX_HEIGHT: f32 = 380.0;

pub struct BrowseChannelsDialog {
    model: Entity<AppModel>,
    trap: ModalTrap,
    filter: Entity<TextInput>,
    highlighted: usize,
    scroll: ScrollHandle,
    /// Click-only handles for the Join and Open buttons on the rows.
    row_focus: HashMap<ConversationId, FocusHandle>,
    joined_focus: FocusHandle,
    sort_focus: FocusHandle,
    more_focus: FocusHandle,
    retry_focus: FocusHandle,
    /// A channel the person asked to join; the dialog opens it once it appears in the sidebar.
    opening: Option<ConversationId>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for BrowseChannelsDialog {}

impl BrowseChannelsDialog {
    /// Opens the directory. With `query`, the core has already been told to search for it (the
    /// quick switcher's "Browse channels matching …"); otherwise the directory opens with the
    /// settings it last had.
    pub fn new(
        model: Entity<AppModel>,
        query: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = {
            cx.new(|cx| {
                TextInput::new(
                    crate::text_input::TextInputOptions::new("Search channels")
                        .accessible_label("Search channels")
                        .single_line(true)
                        .pass_through_navigation(true)
                        .max_bytes(400),
                    cx,
                )
            })
        };
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &filter,
            window,
            |this, input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Changed => {
                    this.highlighted = 0;
                    let text = input.read(cx).text().to_owned();
                    this.dispatch(Input::SetBrowseQuery(text), cx);
                    cx.notify();
                }
                TextInputEvent::Submit => this.activate_highlighted(window, cx),
                TextInputEvent::Tab { backwards } => this.trap.tab(*backwards, window, cx),
                TextInputEvent::UpWhenEmpty
                | TextInputEvent::PasteAttachments
                | TextInputEvent::Suggestion(_) => {}
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &model,
            window,
            |this, _model, event: &AppEvent, window, cx| {
                this.open_if_joined(window, cx);
                let AppEvent::Changed(changes) = event;
                if changes
                    .iter()
                    .any(|change| matches!(change, Change::BrowseChanged))
                {
                    this.highlighted = this.highlighted.min(this.rows(cx).len().saturating_sub(1));
                }
                cx.notify();
            },
        ));
        if query.is_none() {
            model.update(cx, |model, cx| model.dispatch(Input::BrowseChannels, cx));
        }
        // The box shows what the core is searching for.
        let current = model.read(cx).core().browse().query.clone();
        filter.update(cx, |input, cx| input.set_text(&current, cx));
        let mut trap = ModalTrap::new(cx);
        let initial = filter.focus_handle(cx);
        trap.open(&initial, window, cx);
        Self {
            model,
            trap,
            filter,
            highlighted: 0,
            scroll: ScrollHandle::new(),
            row_focus: HashMap::new(),
            joined_focus: cx.focus_handle().tab_stop(true),
            sort_focus: cx.focus_handle().tab_stop(true),
            more_focus: cx.focus_handle().tab_stop(true),
            retry_focus: cx.focus_handle().tab_stop(true),
            opening: None,
            _subscriptions: subscriptions,
        }
    }

    /// The channels listed, for tests.
    pub fn names(&self, cx: &gpui::App) -> Vec<String> {
        self.rows(cx)
            .iter()
            .filter_map(|channel| channel.conversation.name.as_ref().map(ToString::to_string))
            .collect()
    }

    /// Which tab stop has the keyboard, for tests.
    pub fn stop(&self, window: &Window, cx: &gpui::App) -> &'static str {
        if self.filter.read(cx).is_focused(window) {
            "search"
        } else if self.joined_focus.is_focused(window) {
            "filter"
        } else if self.sort_focus.is_focused(window) {
            "sort"
        } else if self.more_focus.is_focused(window) {
            "more"
        } else if self.retry_focus.is_focused(window) {
            "retry"
        } else {
            "elsewhere"
        }
    }

    /// The highlighted row, for tests.
    pub fn highlighted(&self) -> usize {
        self.highlighted
    }

    /// The search box is always there; a button that asked again goes away, so the keyboard
    /// comes here.
    fn focus_search(&self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.filter.focus_handle(cx);
        window.focus(&focus, cx);
    }

    fn dispatch(&self, input: Input, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
    }

    fn rows(&self, cx: &gpui::App) -> Vec<BrowsedChannel> {
        self.model.read(cx).core().browse().channels.clone()
    }

    fn join(&mut self, id: ConversationId, cx: &mut Context<Self>) {
        self.opening = Some(id);
        self.dispatch(Input::JoinChannel(id), cx);
    }

    fn open_channel(&mut self, id: ConversationId, window: &mut Window, cx: &mut Context<Self>) {
        self.dispatch(Input::SelectConversation(id), cx);
        self.finish(window, cx);
    }

    fn activate_highlighted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        let Some(channel) = rows.get(self.highlighted.min(rows.len().saturating_sub(1))) else {
            return;
        };
        let id = channel.conversation.id;
        if channel.joined {
            self.open_channel(id, window, cx);
        } else if self.model.read(cx).core().can_join(channel) {
            self.join(id, cx);
        }
    }

    /// After a join succeeds, the channel is in the sidebar: open it and close the dialog.
    fn open_if_joined(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.opening else { return };
        if self.model.read(cx).core().conversation(id).is_some() {
            self.opening = None;
            self.open_channel(id, window, cx);
        }
    }

    fn move_highlight(&mut self, forwards: bool, cx: &mut Context<Self>) {
        let count = self.rows(cx).len();
        if count == 0 {
            return;
        }
        self.highlighted = if forwards {
            (self.highlighted + 1) % count
        } else {
            (self.highlighted + count - 1) % count
        };
        self.scroll.scroll_to_item(self.highlighted);
        cx.notify();
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if any_composing(&[&self.filter], cx) {
            return;
        }
        self.finish(window, cx);
    }

    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let joins: Vec<ConversationId> = self.row_focus.keys().copied().collect();
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::CloseBrowse, cx);
            for id in joins {
                model.dispatch(
                    Input::DismissAction(lazy_client_core::ActionKind::Join(id)),
                    cx,
                );
            }
        });
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }

    fn controls(
        &self,
        theme: &Theme,
        joined: Option<bool>,
        sort: ChannelSort,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = theme.colors;
        let filter_to = cx.entity().downgrade();
        let sort_to = filter_to.clone();
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(form::toggle_group(
                theme,
                "browse-joined-toggle",
                "Which channels",
                &[("All", "browse-all"), ("Not joined", "browse-not-joined")],
                usize::from(joined == Some(false)),
                &self.joined_focus,
                move |index, _, cx| {
                    filter_to
                        .update(cx, |dialog, cx| {
                            dialog.highlighted = 0;
                            dialog.dispatch(
                                Input::SetBrowseJoined((index == 1).then_some(false)),
                                cx,
                            );
                        })
                        .ok();
                },
            ))
            .child(div().flex_1())
            .child(
                div()
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_muted)
                    .child("Sort"),
            )
            .child(form::toggle_group(
                theme,
                "browse-sort-toggle",
                "Sort channels by",
                &[
                    ("Name", "browse-sort-name"),
                    ("Members", "browse-sort-members"),
                ],
                usize::from(sort == ChannelSort::Members),
                &self.sort_focus,
                move |index, _, cx| {
                    sort_to
                        .update(cx, |dialog, cx| {
                            dialog.highlighted = 0;
                            dialog.dispatch(
                                Input::SetBrowseSort(if index == 1 {
                                    ChannelSort::Members
                                } else {
                                    ChannelSort::Name
                                }),
                                cx,
                            );
                        })
                        .ok();
                },
            ))
            .into_any_element()
    }
}

impl Focusable for BrowseChannelsDialog {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.filter.focus_handle(cx)
    }
}

impl Render for BrowseChannelsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        let (status, joined, sort, has_more, query) = {
            let view = self.model.read(cx).core().browse();
            (
                view.status.clone(),
                view.joined,
                view.sort,
                view.has_more,
                view.query.clone(),
            )
        };
        let rows = self.rows(cx);
        let highlighted = self.highlighted.min(rows.len().saturating_sub(1));
        let this = cx.entity().downgrade();

        let mut list = div()
            .id("browse-list")
            .debug_selector(|| "browse-list".into())
            .role(Role::List)
            .aria_label("Public channels")
            .max_h(px(LIST_MAX_HEIGHT))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .flex()
            .flex_col()
            .gap_0p5();
        for (row, channel) in rows.iter().enumerate() {
            let conversation = &channel.conversation;
            let id = conversation.id;
            let name = conversation
                .name
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            let join_state = self
                .model
                .read(cx)
                .core()
                .action(lazy_client_core::ActionKind::Join(id))
                .clone();
            let can_join = self.model.read(cx).core().can_join(channel);
            let focus = self
                .row_focus
                .entry(id)
                .or_insert_with(|| cx.focus_handle())
                .clone();
            let blurb: SharedString = conversation
                .topic
                .clone()
                .filter(|t| !t.is_empty())
                .or_else(|| conversation.description.clone().filter(|d| !d.is_empty()))
                .unwrap_or_else(|| "No topic yet".to_owned())
                .into();
            let members = format!(
                "{} member{}",
                conversation.member_count,
                if conversation.member_count == 1 {
                    ""
                } else {
                    "s"
                }
            );
            let row_name = format!("browse-row-{name}");
            let join_to = this.clone();
            let open_to = this.clone();
            let hover_to = this.clone();
            let action: gpui::AnyElement = if channel.joined {
                form::button(
                    &theme,
                    format!("browse-open-{name}"),
                    "Open",
                    ButtonKind::Secondary,
                    true,
                    &focus,
                    move |window, cx| {
                        open_to
                            .update(cx, |dialog, cx| dialog.open_channel(id, window, cx))
                            .ok();
                    },
                )
                .into_any_element()
            } else if conversation.archived {
                // An archived channel cannot be joined; the badge beside its name says why.
                div().into_any_element()
            } else {
                form::button(
                    &theme,
                    format!("browse-join-{name}"),
                    if join_state == ActionState::InFlight {
                        "Joining…"
                    } else {
                        "Join"
                    },
                    ButtonKind::Primary,
                    can_join && join_state != ActionState::InFlight,
                    &focus,
                    move |_, cx| {
                        join_to.update(cx, |dialog, cx| dialog.join(id, cx)).ok();
                    },
                )
                .into_any_element()
            };
            list = list.child(
                div()
                    .id(ElementId::Name(row_name.clone().into()))
                    .debug_selector(move || row_name)
                    .role(Role::ListItem)
                    .aria_label(format!("#{name}, {members}"))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_2()
                    .py_1p5()
                    .rounded(theme.radii.small)
                    .when(row == highlighted, |item| item.bg(colors.surface_hover))
                    .on_mouse_move(move |_, _, cx| {
                        hover_to
                            .update(cx, |dialog, cx| {
                                if dialog.highlighted != row {
                                    dialog.highlighted = row;
                                    cx.notify();
                                }
                            })
                            .ok();
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .items_baseline()
                                    .gap_2()
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_size(theme.type_scale.body)
                                            .text_color(colors.text)
                                            .child(format!("# {name}")),
                                    )
                                    .children(conversation.archived.then(|| {
                                        div()
                                            .debug_selector({
                                                let name = name.clone();
                                                move || format!("browse-archived-{name}")
                                            })
                                            .px_1p5()
                                            .rounded(theme.radii.small)
                                            .bg(colors.warning_surface)
                                            .text_color(colors.warning)
                                            .text_size(theme.type_scale.small)
                                            .child("Archived")
                                    }))
                                    .child(
                                        div()
                                            .text_size(theme.type_scale.small)
                                            .text_color(colors.text_faint)
                                            .child(members),
                                    ),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.text_muted)
                                    .child(blurb),
                            )
                            .children(match &join_state {
                                ActionState::Failed(message) => Some(
                                    div()
                                        .text_size(theme.type_scale.small)
                                        .text_color(colors.danger)
                                        .child(message.clone()),
                                ),
                                _ => None,
                            }),
                    )
                    .child(action),
            );
        }

        let retry_first = |dialog: &Self, label: &'static str| {
            let retry_to = this.clone();
            form::button(
                &theme,
                "browse-retry",
                label,
                ButtonKind::Secondary,
                true,
                &dialog.retry_focus,
                move |window, cx| {
                    retry_to
                        .update(cx, |dialog, cx| {
                            dialog.dispatch(Input::BrowseChannels, cx);
                            dialog.focus_search(window, cx);
                        })
                        .ok();
                },
            )
            .into_any_element()
        };
        let not_joined = joined == Some(false);
        // What takes the list's place when there is nothing to list.
        let empty_note: Option<gpui::AnyElement> = if rows.is_empty() {
            Some(match &status {
                LoadStatus::Idle | LoadStatus::Loading => div()
                    .id("browse-loading")
                    .debug_selector(|| "browse-loading".into())
                    .role(Role::Status)
                    .text_color(colors.text_muted)
                    .text_size(theme.type_scale.body)
                    .child("Loading channels…")
                    .into_any_element(),
                LoadStatus::Failed { reason, can_retry } => div()
                    .id("browse-failed")
                    .debug_selector(|| "browse-failed".into())
                    .role(Role::Alert)
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(theme.type_scale.body)
                            .text_color(colors.text)
                            .child(states::BROWSE_FAILED_TITLE),
                    )
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap_3()
                            .child(
                                div()
                                    // Wraps in the room the button leaves, so a long reason
                                    // cannot push it out of the dialog.
                                    .flex_1()
                                    .min_w_0()
                                    .text_color(colors.danger)
                                    .text_size(theme.type_scale.body)
                                    .child(states::sentence(reason)),
                            )
                            .children(can_retry.then(|| retry_first(self, states::TRY_AGAIN))),
                    )
                    .into_any_element(),
                LoadStatus::Ready => div()
                    .id("browse-empty")
                    .debug_selector(|| "browse-empty".into())
                    .text_color(colors.text_muted)
                    .text_size(theme.type_scale.body)
                    .child(states::browse_no_match(&query, not_joined))
                    .into_any_element(),
            })
        } else {
            None
        };
        // Under the list: the next page loading, failing, or there to ask for.
        let footer: Option<gpui::AnyElement> = if rows.is_empty() {
            None
        } else {
            match &status {
                LoadStatus::Loading => Some(
                    div()
                        .debug_selector(|| "browse-loading-more".into())
                        .text_size(theme.type_scale.small)
                        .text_color(colors.text_muted)
                        .child("Loading more channels…")
                        .into_any_element(),
                ),
                LoadStatus::Failed { reason, can_retry } => {
                    let more_to = this.clone();
                    Some(
                        div()
                            .debug_selector(|| "browse-more-failed".into())
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.danger)
                                    .child(states::browse_more_failed(reason)),
                            )
                            .children(can_retry.then(|| {
                                form::button(
                                    &theme,
                                    "browse-retry",
                                    states::TRY_AGAIN,
                                    ButtonKind::Secondary,
                                    true,
                                    &self.retry_focus,
                                    move |window, cx| {
                                        more_to
                                            .update(cx, |dialog, cx| {
                                                dialog.dispatch(Input::LoadMoreBrowse, cx);
                                                dialog.focus_search(window, cx);
                                            })
                                            .ok();
                                    },
                                )
                            }))
                            .into_any_element(),
                    )
                }
                LoadStatus::Ready | LoadStatus::Idle if has_more => {
                    let more_to = this.clone();
                    Some(
                        form::button(
                            &theme,
                            "browse-load-more",
                            "Load more",
                            ButtonKind::Secondary,
                            true,
                            &self.more_focus,
                            move |_, cx| {
                                more_to
                                    .update(cx, |dialog, cx| {
                                        dialog.dispatch(Input::LoadMoreBrowse, cx)
                                    })
                                    .ok();
                            },
                        )
                        .into_any_element(),
                    )
                }
                _ => None,
            }
        };

        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded(theme.radii.medium)
                    .bg(colors.surface_raised)
                    .border_1()
                    .border_color(if self.filter.read(cx).is_focused(window) {
                        colors.focus_ring
                    } else {
                        colors.border_strong
                    })
                    .child(self.filter.clone()),
            )
            .child(self.controls(&theme, joined, sort, cx))
            .children(empty_note)
            .when(!rows.is_empty(), |body| body.child(list))
            .children(footer)
            .into_any_element();

        let close_to = this.clone();
        dialog_overlay(
            &theme,
            "Browse channels",
            600.,
            &self.trap,
            body,
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
        .on_action(cx.listener(|this, _: &MoveDown, _, cx| this.move_highlight(true, cx)))
        .on_action(cx.listener(|this, _: &MoveUp, _, cx| this.move_highlight(false, cx)))
    }
}
