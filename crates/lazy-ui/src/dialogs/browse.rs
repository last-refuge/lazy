//! Browse channels: the directory of public channels, filterable, with Join.

use std::collections::HashMap;

use gpui::{
    Context, ElementId, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled, Subscription,
    Window, div, prelude::*,
};
use lazy_client_core::{ActionKind, ActionState, Input};
use lazy_domain::ConversationId;
use lazy_protocol::BrowsedChannel;

use super::any_composing;
use crate::app_model::{AppEvent, AppModel};
use crate::form::{self, ButtonKind};
use crate::modal::{DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, dialog_overlay};
use crate::text_input::actions::{MoveDown, MoveUp};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::Theme;

/// Rows shown at once; more are reached by typing in the filter box.
const VISIBLE_ROWS: usize = 8;

pub struct BrowseChannelsDialog {
    model: Entity<AppModel>,
    trap: ModalTrap,
    filter: Entity<TextInput>,
    highlighted: usize,
    join_focus: HashMap<ConversationId, FocusHandle>,
    retry_focus: FocusHandle,
    /// A channel the person asked to join; the dialog opens it once it appears in the sidebar.
    opening: Option<ConversationId>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for BrowseChannelsDialog {}

impl BrowseChannelsDialog {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = {
            cx.new(|cx| {
                TextInput::new(
                    crate::text_input::TextInputOptions::new("Filter channels")
                        .accessible_label("Filter channels")
                        .single_line(true)
                        .pass_through_navigation(true)
                        .max_bytes(120),
                    cx,
                )
            })
        };
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &filter,
            window,
            |this, _input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Changed => {
                    this.highlighted = 0;
                    cx.notify();
                }
                TextInputEvent::Submit => this.activate_highlighted(window, cx),
                TextInputEvent::Tab { backwards } => this.trap.tab(*backwards, window, cx),
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &model,
            window,
            |this, _model, _event: &AppEvent, window, cx| {
                this.open_if_joined(window, cx);
                cx.notify();
            },
        ));
        model.update(cx, |model, cx| model.dispatch(Input::BrowseChannels, cx));
        let mut trap = ModalTrap::new(cx);
        let initial = filter.focus_handle(cx);
        trap.open(&initial, window, cx);
        Self {
            model,
            trap,
            filter,
            highlighted: 0,
            join_focus: HashMap::new(),
            retry_focus: cx.focus_handle().tab_stop(true),
            opening: None,
            _subscriptions: subscriptions,
        }
    }

    /// The channels that match the filter, in directory order.
    fn visible(&self, cx: &gpui::App) -> Vec<BrowsedChannel> {
        let needle = self.filter.read(cx).text().trim().to_lowercase();
        self.model
            .read(cx)
            .core()
            .browse_results()
            .iter()
            .filter(|channel| {
                needle.is_empty() || {
                    let c = &channel.conversation;
                    c.name
                        .as_ref()
                        .is_some_and(|n| n.as_str().contains(&needle))
                        || c.topic
                            .as_ref()
                            .is_some_and(|t| t.to_lowercase().contains(&needle))
                        || c.description
                            .as_ref()
                            .is_some_and(|d| d.to_lowercase().contains(&needle))
                }
            })
            .cloned()
            .collect()
    }

    fn join(&mut self, id: ConversationId, cx: &mut Context<Self>) {
        self.opening = Some(id);
        self.model
            .update(cx, |model, cx| model.dispatch(Input::JoinChannel(id), cx));
    }

    fn open_channel(&mut self, id: ConversationId, window: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::SelectConversation(id), cx)
        });
        self.finish(window, cx);
    }

    fn activate_highlighted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.visible(cx);
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
        let count = self.visible(cx).len().min(VISIBLE_ROWS);
        if count == 0 {
            return;
        }
        self.highlighted = if forwards {
            (self.highlighted + 1) % count
        } else {
            (self.highlighted + count - 1) % count
        };
        cx.notify();
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if any_composing(&[&self.filter], cx) {
            return;
        }
        self.finish(window, cx);
    }

    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let joins: Vec<ConversationId> = self.join_focus.keys().copied().collect();
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::DismissAction(ActionKind::Browse), cx);
            for id in joins {
                model.dispatch(Input::DismissAction(ActionKind::Join(id)), cx);
            }
        });
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }
}

impl Render for BrowseChannelsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        let browse = self
            .model
            .read(cx)
            .core()
            .action(ActionKind::Browse)
            .clone();
        let rows = self.visible(cx);
        let total = rows.len();
        let shown: Vec<BrowsedChannel> = rows.into_iter().take(VISIBLE_ROWS).collect();
        let highlighted = self.highlighted.min(shown.len().saturating_sub(1));
        let this = cx.entity().downgrade();

        let mut list = div().flex().flex_col().gap_0p5();
        for (row, channel) in shown.iter().enumerate() {
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
                .action(ActionKind::Join(id))
                .clone();
            let can_join = self.model.read(cx).core().can_join(channel);
            let focus = self
                .join_focus
                .entry(id)
                .or_insert_with(|| cx.focus_handle().tab_stop(true))
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
                div()
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_faint)
                    .child("Archived")
                    .into_any_element()
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
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_2()
                    .py_1p5()
                    .rounded(theme.radii.small)
                    .when(row == highlighted, |item| item.bg(colors.surface_hover))
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

        let status: Option<gpui::AnyElement> = match &browse {
            ActionState::InFlight if shown.is_empty() => Some(
                div()
                    .text_color(colors.text_muted)
                    .text_size(theme.type_scale.body)
                    .child("Loading channels…")
                    .into_any_element(),
            ),
            ActionState::Failed(message) => {
                let retry_to = this.clone();
                Some(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .text_color(colors.danger)
                                .text_size(theme.type_scale.body)
                                .child(message.clone()),
                        )
                        .child(form::button(
                            &theme,
                            "browse-retry",
                            "Try again",
                            ButtonKind::Secondary,
                            true,
                            &self.retry_focus,
                            move |_, cx| {
                                retry_to
                                    .update(cx, |dialog, cx| {
                                        dialog.model.update(cx, |model, cx| {
                                            model.dispatch(Input::BrowseChannels, cx)
                                        });
                                    })
                                    .ok();
                            },
                        ))
                        .into_any_element(),
                )
            }
            _ if shown.is_empty() => Some(
                div()
                    .text_color(colors.text_muted)
                    .text_size(theme.type_scale.body)
                    .child(if self.filter.read(cx).text().trim().is_empty() {
                        "There are no public channels to browse yet. Create one from the sidebar."
                    } else {
                        "No channels match that."
                    })
                    .into_any_element(),
            ),
            _ => None,
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
            .children(status)
            .child(list)
            .when(total > VISIBLE_ROWS, |body| {
                body.child(
                    div()
                        .text_size(theme.type_scale.small)
                        .text_color(colors.text_muted)
                        .child(format!(
                            "Showing {VISIBLE_ROWS} of {total}. Keep typing to narrow the list."
                        )),
                )
            })
            .into_any_element();

        let close_to = this.clone();
        dialog_overlay(
            &theme,
            "Browse channels",
            560.,
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
