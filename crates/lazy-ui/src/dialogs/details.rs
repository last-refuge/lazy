//! Conversation details: name, topic, and description for channels (editable by those allowed),
//! the participants of a direct or group conversation, and the management actions. Destructive
//! actions (leave, archive) ask for confirmation inside the same dialog.

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, ParentElement,
    Render, Styled, Subscription, Window, div, prelude::*,
};
use lazy_client_core::{ActionKind, ActionState, Input};
use lazy_domain::{ChannelVisibility, ConversationId, ConversationKind, UserId};

use super::{any_composing, multiline_field, single_line_field};
use crate::app_model::{AppEvent, AppModel};
use crate::form::{self, ButtonKind};
use crate::modal::{DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, dialog_overlay};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::Theme;

/// Something the details dialog asks its host to open after it closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailsRequest {
    /// Open the people dialog to add people to this channel.
    AddPeopleToChannel(ConversationId),
    /// Open the people dialog to start a *new* group with these people (everyone here plus more).
    StartLargerGroup {
        from: ConversationId,
        prefill: Vec<UserId>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Confirm {
    Leave,
    Archive,
    Unarchive,
}

pub struct DetailsDialog {
    model: Entity<AppModel>,
    id: ConversationId,
    trap: ModalTrap,
    name: Entity<TextInput>,
    topic: Entity<TextInput>,
    description: Entity<TextInput>,
    confirming: Option<Confirm>,
    /// The action whose completion closes the dialog, and whether it has started.
    waiting: Option<(ActionKind, bool)>,
    save_focus: FocusHandle,
    add_focus: FocusHandle,
    leave_focus: FocusHandle,
    archive_focus: FocusHandle,
    close_focus: FocusHandle,
    confirm_cancel_focus: FocusHandle,
    confirm_ok_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for DetailsDialog {}
impl EventEmitter<DetailsRequest> for DetailsDialog {}

impl DetailsDialog {
    pub fn new(
        model: Entity<AppModel>,
        id: ConversationId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = single_line_field("channel-name", "Channel name", 120, cx);
        let topic = single_line_field("Add a topic", "Topic", 250, cx);
        let description =
            multiline_field("Describe what this channel is for", "Description", 1000, cx);
        if let Some(conversation) = model.read(cx).core().conversation(id).cloned() {
            let name_text = conversation
                .name
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            name.update(cx, |input, cx| input.set_text(&name_text, cx));
            topic.update(cx, |input, cx| {
                input.set_text(conversation.topic.as_deref().unwrap_or(""), cx)
            });
            description.update(cx, |input, cx| {
                input.set_text(conversation.description.as_deref().unwrap_or(""), cx);
            });
        }
        let mut subscriptions = Vec::new();
        for input in [&name, &topic] {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                |this, _input, event: &TextInputEvent, window, cx| match event {
                    TextInputEvent::Submit => this.save(cx),
                    TextInputEvent::Tab { backwards } => this.trap.tab(*backwards, window, cx),
                    TextInputEvent::Changed => cx.notify(),
                },
            ));
        }
        subscriptions.push(cx.subscribe(
            &description,
            |this, _input, event: &TextInputEvent, cx| match event {
                TextInputEvent::Submit => this.save(cx),
                TextInputEvent::Changed => cx.notify(),
                TextInputEvent::Tab { .. } => {}
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &model,
            window,
            |this, _model, _event: &AppEvent, window, cx| {
                this.settle(window, cx);
                cx.notify();
            },
        ));
        let mut trap = ModalTrap::new(cx);
        let is_channel = model
            .read(cx)
            .core()
            .conversation(id)
            .is_some_and(|c| c.kind == ConversationKind::Channel);
        let initial = if is_channel && model.read(cx).core().can_edit_details(id) {
            topic.focus_handle(cx)
        } else {
            cx.focus_handle().tab_stop(true)
        };
        let close_focus = cx.focus_handle().tab_stop(true);
        trap.open(
            if is_channel && model.read(cx).core().can_edit_details(id) {
                &initial
            } else {
                &close_focus
            },
            window,
            cx,
        );
        Self {
            model,
            id,
            trap,
            name,
            topic,
            description,
            confirming: None,
            waiting: None,
            save_focus: cx.focus_handle().tab_stop(true),
            add_focus: cx.focus_handle().tab_stop(true),
            leave_focus: cx.focus_handle().tab_stop(true),
            archive_focus: cx.focus_handle().tab_stop(true),
            close_focus,
            confirm_cancel_focus: cx.focus_handle().tab_stop(true),
            confirm_ok_focus: cx.focus_handle().tab_stop(true),
            _subscriptions: subscriptions,
        }
    }

    /// Closes the dialog once the action it was waiting for has succeeded.
    fn settle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let gone = self.model.read(cx).core().conversation(self.id).is_none();
        if gone {
            self.finish(window, cx);
            return;
        }
        let Some((kind, started)) = self.waiting else {
            return;
        };
        let state = self.model.read(cx).core().action(kind).clone();
        match state {
            ActionState::InFlight => self.waiting = Some((kind, true)),
            ActionState::Idle if started => self.finish(window, cx),
            ActionState::Failed(_) => {
                self.waiting = None;
                self.confirming = None;
            }
            ActionState::Idle => {}
        }
    }

    fn changed(&self, cx: &gpui::App) -> (Option<String>, Option<String>, Option<String>) {
        let Some(conversation) = self.model.read(cx).core().conversation(self.id) else {
            return (None, None, None);
        };
        let core = self.model.read(cx).core();
        let diff = |current: &str, typed: &str| (current != typed).then(|| typed.to_owned());
        let name = if core.can_rename(self.id) {
            diff(
                conversation.name.as_ref().map_or("", |n| n.as_str()),
                self.name.read(cx).text().trim(),
            )
        } else {
            None
        };
        let (topic, description) = if core.can_edit_details(self.id) {
            (
                diff(
                    conversation.topic.as_deref().unwrap_or(""),
                    self.topic.read(cx).text().trim(),
                ),
                diff(
                    conversation.description.as_deref().unwrap_or(""),
                    self.description.read(cx).text().trim(),
                ),
            )
        } else {
            (None, None)
        };
        (name, topic, description)
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let (name, topic, description) = self.changed(cx);
        if name.is_none() && topic.is_none() && description.is_none() {
            return;
        }
        let id = self.id;
        self.waiting = Some((ActionKind::UpdateConversation(id), false));
        self.model.update(cx, |model, cx| {
            model.dispatch(
                Input::UpdateConversation {
                    id,
                    name,
                    topic,
                    description,
                },
                cx,
            );
        });
    }

    fn confirm(&mut self, what: Confirm, cx: &mut Context<Self>) {
        let id = self.id;
        let (kind, input) = match what {
            Confirm::Leave => (ActionKind::Leave(id), Input::LeaveChannel(id)),
            Confirm::Archive => (ActionKind::Archive(id), Input::ArchiveConversation(id)),
            Confirm::Unarchive => (ActionKind::Unarchive(id), Input::UnarchiveConversation(id)),
        };
        self.waiting = Some((kind, false));
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if any_composing(&[&self.name, &self.topic, &self.description], cx) {
            return;
        }
        if self.confirming.take().is_some() {
            cx.notify();
            return;
        }
        self.finish(window, cx);
    }

    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.id;
        self.model.update(cx, |model, cx| {
            for kind in [
                ActionKind::UpdateConversation(id),
                ActionKind::Leave(id),
                ActionKind::Archive(id),
                ActionKind::Unarchive(id),
            ] {
                model.dispatch(Input::DismissAction(kind), cx);
            }
        });
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }

    /// Closes this dialog and asks the host to open another.
    ///
    /// The request is emitted first: the host's subscription to it lives in the overlay, which
    /// is dropped when the host handles the dismissal, so the dismissal must come second. By the
    /// time either event is handled, `finish` has already restored focus, so the next dialog
    /// remembers the right control to return to.
    fn request(&mut self, request: DetailsRequest, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(request);
        self.finish(window, cx);
    }
}

impl Render for DetailsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let close_to = this.clone();
        let id = self.id;
        let core = self.model.read(cx).core();
        let Some(conversation) = core.conversation(id).cloned() else {
            // Removed while open: `settle` closes the dialog on the next change.
            return dialog_overlay(
                &theme,
                "Conversation",
                480.,
                &self.trap,
                div().into_any_element(),
                "Close",
                move |window, cx| {
                    close_to
                        .update(cx, |dialog, cx| dialog.finish(window, cx))
                        .ok();
                },
            );
        };
        let title = core.conversation_title(id);
        let is_channel = conversation.kind == ConversationKind::Channel;
        let (can_rename, can_edit, can_add, can_leave, can_archive, can_unarchive) = (
            core.can_rename(id),
            core.can_edit_details(id),
            core.can_add_members(id),
            core.can_leave(id),
            core.can_archive(id),
            core.can_unarchive(id),
        );
        let participants: Vec<String> = conversation
            .participants
            .iter()
            .map(|user| core.display_name(*user).to_owned())
            .collect();
        let prefill: Vec<UserId> = conversation
            .participants
            .iter()
            .copied()
            .filter(|user| Some(*user) != core.me().map(|me| me.id))
            .collect();
        let update_state = core.action(ActionKind::UpdateConversation(id)).clone();
        let (name_change, topic_change, description_change) = self.changed(cx);
        let dirty = name_change.is_some() || topic_change.is_some() || description_change.is_some();
        let saving = update_state == ActionState::InFlight;
        let error: Option<String> = [
            ActionKind::UpdateConversation(id),
            ActionKind::Leave(id),
            ActionKind::Archive(id),
            ActionKind::Unarchive(id),
        ]
        .into_iter()
        .find_map(|kind| match self.model.read(cx).core().action(kind) {
            ActionState::Failed(message) => Some(message.clone()),
            _ => None,
        });

        let facts_line = if is_channel {
            format!(
                "{} channel · {} member{}{}",
                if conversation.visibility == ChannelVisibility::Private {
                    "Private"
                } else {
                    "Public"
                },
                conversation.member_count,
                if conversation.member_count == 1 {
                    ""
                } else {
                    "s"
                },
                if conversation.archived {
                    " · Archived"
                } else {
                    ""
                },
            )
        } else if conversation.kind == ConversationKind::Direct {
            "Direct message".to_owned()
        } else {
            format!("Group conversation · {} people", participants.len())
        };

        let readonly = |label: &'static str, text: String| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(theme.type_scale.small)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.text_muted)
                        .child(label),
                )
                .child(
                    div()
                        .text_size(theme.type_scale.body)
                        .text_color(if text.is_empty() {
                            colors.text_faint
                        } else {
                            colors.text
                        })
                        .child(if text.is_empty() {
                            "Not set".to_owned()
                        } else {
                            text
                        }),
                )
        };

        let body = if let Some(confirming) = self.confirming {
            let (message, label, kind) = match confirming {
                Confirm::Leave => (
                    format!(
                        "Leave #{title}? You can rejoin later if it is public; a private channel needs a new invitation."
                    ),
                    "Leave channel",
                    ButtonKind::Danger,
                ),
                Confirm::Archive => (
                    format!(
                        "Archive #{title}? It becomes read-only for everyone, and you can unarchive it later."
                    ),
                    "Archive channel",
                    ButtonKind::Danger,
                ),
                Confirm::Unarchive => (
                    format!("Unarchive #{title}? People will be able to post in it again."),
                    "Unarchive channel",
                    ButtonKind::Primary,
                ),
            };
            let cancel_to = this.clone();
            let ok_to = this.clone();
            let working =
                self.waiting.is_some() && update_working(self.model.read(cx).core(), self.waiting);
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .text_size(theme.type_scale.body)
                        .text_color(colors.text)
                        .child(message),
                )
                .children(error.clone().map(|e| {
                    div()
                        .text_size(theme.type_scale.small)
                        .text_color(colors.danger)
                        .child(e)
                }))
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(form::button(
                            &theme,
                            "details-confirm-cancel",
                            "Cancel",
                            ButtonKind::Secondary,
                            true,
                            &self.confirm_cancel_focus,
                            move |_, cx| {
                                cancel_to
                                    .update(cx, |dialog, cx| {
                                        dialog.confirming = None;
                                        cx.notify();
                                    })
                                    .ok();
                            },
                        ))
                        .child(form::button(
                            &theme,
                            "details-confirm-ok",
                            if working { "Working…" } else { label },
                            kind,
                            !working,
                            &self.confirm_ok_focus,
                            move |_, cx| {
                                ok_to
                                    .update(cx, |dialog, cx| dialog.confirm(confirming, cx))
                                    .ok();
                            },
                        )),
                )
                .into_any_element()
        } else {
            let mut body = div().flex().flex_col().gap_3();
            body = body.child(
                div()
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_muted)
                    .child(facts_line),
            );
            if is_channel {
                body = body.child(if can_rename {
                    form::labeled_field(&theme, "Name", &self.name, None, None, window, cx)
                } else {
                    readonly("Name", format!("#{title}")).into_any_element()
                });
                body = body.child(if can_edit {
                    form::labeled_field(&theme, "Topic", &self.topic, None, None, window, cx)
                } else {
                    readonly("Topic", conversation.topic.clone().unwrap_or_default())
                        .into_any_element()
                });
                body = body.child(if can_edit {
                    form::labeled_field(
                        &theme,
                        "Description",
                        &self.description,
                        Some("Ctrl+Enter saves.".into()),
                        None,
                        window,
                        cx,
                    )
                } else {
                    readonly(
                        "Description",
                        conversation.description.clone().unwrap_or_default(),
                    )
                    .into_any_element()
                });
            } else {
                body = body.child(readonly("People", participants.join(", ")));
            }
            body = body.children(error.clone().map(|e| {
                div()
                    .text_size(theme.type_scale.small)
                    .text_color(colors.danger)
                    .child(e)
            }));

            let mut actions = div().flex().flex_wrap().gap_2();
            if is_channel && can_add {
                let add_to = this.clone();
                actions = actions.child(form::button(
                    &theme,
                    "details-add-people",
                    "Add people",
                    ButtonKind::Secondary,
                    true,
                    &self.add_focus,
                    move |window, cx| {
                        add_to
                            .update(cx, |dialog, cx| {
                                dialog.request(DetailsRequest::AddPeopleToChannel(id), window, cx)
                            })
                            .ok();
                    },
                ));
            }
            if !is_channel {
                let add_to = this.clone();
                let prefill = prefill.clone();
                actions = actions.child(form::button(
                    &theme,
                    "details-add-people",
                    "Add people…",
                    ButtonKind::Secondary,
                    true,
                    &self.add_focus,
                    move |window, cx| {
                        let prefill = prefill.clone();
                        add_to
                            .update(cx, |dialog, cx| {
                                dialog.request(
                                    DetailsRequest::StartLargerGroup { from: id, prefill },
                                    window,
                                    cx,
                                )
                            })
                            .ok();
                    },
                ));
            }
            if is_channel && can_leave {
                let leave_to = this.clone();
                actions = actions.child(form::button(
                    &theme,
                    "details-leave",
                    "Leave channel",
                    ButtonKind::Danger,
                    true,
                    &self.leave_focus,
                    move |window, cx| {
                        leave_to
                            .update(cx, |dialog, cx| {
                                dialog.confirming = Some(Confirm::Leave);
                                window.focus(&dialog.confirm_cancel_focus, cx);
                                cx.notify();
                            })
                            .ok();
                    },
                ));
            }
            if is_channel && (can_archive || can_unarchive) {
                let archive_to = this.clone();
                let what = if can_unarchive {
                    Confirm::Unarchive
                } else {
                    Confirm::Archive
                };
                actions = actions.child(form::button(
                    &theme,
                    "details-archive",
                    if can_unarchive {
                        "Unarchive channel"
                    } else {
                        "Archive channel"
                    },
                    if can_unarchive {
                        ButtonKind::Secondary
                    } else {
                        ButtonKind::Danger
                    },
                    true,
                    &self.archive_focus,
                    move |window, cx| {
                        archive_to
                            .update(cx, |dialog, cx| {
                                dialog.confirming = Some(what);
                                window.focus(&dialog.confirm_cancel_focus, cx);
                                cx.notify();
                            })
                            .ok();
                    },
                ));
            }
            let save_to = this.clone();
            let close_btn_to = this.clone();
            body.child(actions)
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(form::button(
                            &theme,
                            "details-close",
                            "Close",
                            ButtonKind::Secondary,
                            true,
                            &self.close_focus,
                            move |window, cx| {
                                close_btn_to
                                    .update(cx, |dialog, cx| dialog.dismiss(window, cx))
                                    .ok();
                            },
                        ))
                        .when(is_channel && (can_rename || can_edit), |row| {
                            row.child(form::button(
                                &theme,
                                "details-save",
                                if saving { "Saving…" } else { "Save changes" },
                                ButtonKind::Primary,
                                dirty && !saving,
                                &self.save_focus,
                                move |_, cx| {
                                    save_to.update(cx, |dialog, cx| dialog.save(cx)).ok();
                                },
                            ))
                        }),
                )
                .into_any_element()
        };

        let heading = if is_channel {
            format!("#{title}")
        } else {
            title
        };
        dialog_overlay(
            &theme,
            heading,
            500.,
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
    }
}

/// Whether the awaited action is currently in flight.
fn update_working(
    core: &lazy_client_core::ClientCore,
    waiting: Option<(ActionKind, bool)>,
) -> bool {
    waiting.is_some_and(|(kind, _)| core.action(kind) == &ActionState::InFlight)
}
