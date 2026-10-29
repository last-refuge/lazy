//! Create channel: a name (guided into a valid slug), a topic, and public or private.

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    SharedString, Styled, Subscription, Window, div, prelude::*,
};
use lazy_client_core::{ActionKind, ActionState, Change, Input};
use lazy_domain::ChannelVisibility;

use super::{any_composing, single_line_field};
use crate::app_model::{AppEvent, AppModel};
use crate::channel_name::{NameGuidance, guidance};
use crate::form::{self, ButtonKind, Segment};
use crate::modal::{DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, dialog_overlay};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::Theme;

pub struct CreateChannelDialog {
    model: Entity<AppModel>,
    trap: ModalTrap,
    name: Entity<TextInput>,
    topic: Entity<TextInput>,
    visibility: ChannelVisibility,
    visibility_focus: FocusHandle,
    cancel_focus: FocusHandle,
    create_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for CreateChannelDialog {}

impl CreateChannelDialog {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = single_line_field("e.g. design-team", "Channel name", 120, cx);
        let topic = single_line_field("What is this channel about? (optional)", "Topic", 250, cx);
        let mut subscriptions = Vec::new();
        for input in [&name, &topic] {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                |this, _input, event: &TextInputEvent, window, cx| match event {
                    TextInputEvent::Submit => this.create(cx),
                    TextInputEvent::Tab { backwards } => this.trap.tab(*backwards, window, cx),
                    TextInputEvent::UpWhenEmpty
                    | TextInputEvent::PasteAttachments
                    | TextInputEvent::Suggestion(_) => {}
                    TextInputEvent::Changed => cx.notify(),
                },
            ));
        }
        subscriptions.push(cx.subscribe_in(
            &model,
            window,
            |this, _model, event: &AppEvent, window, cx| {
                let AppEvent::Changed(changes) = event;
                if changes
                    .iter()
                    .any(|c| matches!(c, Change::ConversationOpened(_)))
                {
                    // The new channel is ready and selected: the dialog's job is done.
                    this.finish(window, cx);
                } else {
                    cx.notify();
                }
            },
        ));
        let mut trap = ModalTrap::new(cx);
        let initial = name.focus_handle(cx);
        trap.open(&initial, window, cx);
        Self {
            model,
            trap,
            name,
            topic,
            visibility: ChannelVisibility::Public,
            visibility_focus: cx.focus_handle().tab_stop(true),
            cancel_focus: cx.focus_handle().tab_stop(true),
            create_focus: cx.focus_handle().tab_stop(true),
            _subscriptions: subscriptions,
        }
    }

    fn state(&self, cx: &gpui::App) -> ActionState {
        self.model
            .read(cx)
            .core()
            .action(ActionKind::CreateChannel)
            .clone()
    }

    /// The name to send: as typed when valid, else the cleaned-up slug.
    fn chosen_name(&self, cx: &gpui::App) -> Option<String> {
        let text = self.name.read(cx).text().to_owned();
        match guidance(&text) {
            NameGuidance::Valid => Some(text),
            NameGuidance::WillBecome(slug) => Some(slug),
            NameGuidance::Empty | NameGuidance::Unusable => None,
        }
    }

    fn create(&mut self, cx: &mut Context<Self>) {
        if self.state(cx) == ActionState::InFlight {
            return;
        }
        let Some(name) = self.chosen_name(cx) else {
            return;
        };
        let topic = self.topic.read(cx).text().trim().to_owned();
        let visibility = self.visibility;
        self.model.update(cx, |model, cx| {
            model.dispatch(
                Input::CreateChannel {
                    name,
                    visibility,
                    topic,
                    description: String::new(),
                },
                cx,
            );
        });
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if any_composing(&[&self.name, &self.topic], cx) {
            return;
        }
        self.finish(window, cx);
    }

    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::DismissAction(ActionKind::CreateChannel), cx);
        });
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }
}

impl Render for CreateChannelDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let state = self.state(cx);
        let in_flight = state == ActionState::InFlight;
        let typed = self.name.read(cx).text().to_owned();
        let (hint, error): (Option<SharedString>, Option<SharedString>) = match (
            &state,
            guidance(&typed),
        ) {
            (ActionState::Failed(message), _) => (None, Some(message.clone().into())),
            (_, NameGuidance::Empty) => (
                Some(
                    "Lowercase letters, numbers, dashes, and underscores. Spaces become dashes."
                        .into(),
                ),
                None,
            ),
            (_, NameGuidance::Valid) => (Some(format!("Will be created as #{typed}").into()), None),
            (_, NameGuidance::WillBecome(slug)) => {
                (Some(format!("Will be created as #{slug}").into()), None)
            }
            (_, NameGuidance::Unusable) => {
                (None, Some("Use at least one letter or number.".into()))
            }
        };
        let can_create = self.chosen_name(cx).is_some() && !in_flight;

        let this = cx.entity().downgrade();
        let segments = [
            Segment {
                label: "Public".into(),
                detail: "Anyone in the workspace can join".into(),
            },
            Segment {
                label: "Private".into(),
                detail: "Only people you invite".into(),
            },
        ];
        let selected = usize::from(self.visibility == ChannelVisibility::Private);
        let cancel_to = this.clone();
        let create_to = this.clone();
        let select_to = this.clone();
        let close_to = this.clone();

        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(form::labeled_field(
                &theme, "Name", &self.name, hint, error, window, cx,
            ))
            .child(form::labeled_field(
                &theme,
                "Topic",
                &self.topic,
                None,
                None,
                window,
                cx,
            ))
            .child(form::segmented(
                &theme,
                "visibility",
                &segments,
                selected,
                &self.visibility_focus,
                move |index, _, cx| {
                    select_to
                        .update(cx, |dialog, cx| {
                            dialog.visibility = if index == 1 {
                                ChannelVisibility::Private
                            } else {
                                ChannelVisibility::Public
                            };
                            cx.notify();
                        })
                        .ok();
                },
            ))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        &theme,
                        "create-channel-cancel",
                        "Cancel",
                        ButtonKind::Secondary,
                        true,
                        &self.cancel_focus,
                        move |window, cx| {
                            cancel_to
                                .update(cx, |dialog, cx| dialog.dismiss(window, cx))
                                .ok();
                        },
                    ))
                    .child(form::button(
                        &theme,
                        "create-channel-submit",
                        if in_flight {
                            "Creating…"
                        } else {
                            "Create channel"
                        },
                        ButtonKind::Primary,
                        can_create,
                        &self.create_focus,
                        move |_, cx| {
                            create_to.update(cx, |dialog, cx| dialog.create(cx)).ok();
                        },
                    )),
            )
            .into_any_element();

        dialog_overlay(
            &theme,
            "Create a channel",
            440.,
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
