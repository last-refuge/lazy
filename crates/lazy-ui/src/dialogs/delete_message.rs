//! Asks before deleting a message. Deleting removes the content for everyone and cannot be undone,
//! so the dialog says that, shows what is about to go, and puts focus on Cancel.

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled,
    Subscription, Window, div, prelude::*, px,
};
use lazy_client_core::{ActionKind, ActionState, Input};
use lazy_domain::{ConversationId, MessageId};

use crate::app_model::{AppEvent, AppModel};
use crate::form::{self, ButtonKind};
use crate::modal::{DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, dialog_overlay};
use crate::theme::Theme;

/// Longest excerpt of the message shown in the dialog, in characters.
const PREVIEW_CHARS: usize = 240;

pub struct DeleteMessageDialog {
    model: Entity<AppModel>,
    conversation: ConversationId,
    message: MessageId,
    /// What the message said when the dialog opened, shortened. Kept because the message leaves
    /// the timeline the moment the deletion succeeds.
    preview: String,
    /// How many replies the message has when it starts a thread: they stay when it is deleted.
    replies: u32,
    trap: ModalTrap,
    /// Whether the deletion has been requested; it ends the dialog once it settles.
    waiting: bool,
    started: bool,
    cancel_focus: FocusHandle,
    delete_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for DeleteMessageDialog {}

impl DeleteMessageDialog {
    pub fn new(
        model: Entity<AppModel>,
        conversation: ConversationId,
        message: MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (preview, replies) = {
            let core = model.read(cx).core();
            let row = core
                .timeline(conversation)
                .iter()
                .find(|item| item.message_id() == Some(message));
            // A reply is not in the conversation's rows, only in its thread's: the core can
            // still quote it.
            let preview = row
                .map(|item| excerpt(&item.document.plain_text()))
                .or_else(|| {
                    core.message_excerpt(conversation, message)
                        .map(|quoted| excerpt(&quoted.text))
                })
                .unwrap_or_default();
            let replies = row
                .and_then(|item| item.thread.as_ref())
                .map_or(0, |summary| summary.reply_count);
            (preview, replies)
        };
        let subscription = cx.subscribe_in(
            &model,
            window,
            |this, _model, _event: &AppEvent, window, cx| {
                this.settle(window, cx);
                cx.notify();
            },
        );
        let mut trap = ModalTrap::new(cx);
        let cancel_focus = cx.focus_handle().tab_stop(true);
        trap.open(&cancel_focus, window, cx);
        Self {
            model,
            conversation,
            message,
            preview,
            replies,
            trap,
            waiting: false,
            started: false,
            cancel_focus,
            delete_focus: cx.focus_handle().tab_stop(true),
            _subscriptions: vec![subscription],
        }
    }

    fn kind(&self) -> ActionKind {
        ActionKind::DeleteMessage(self.message)
    }

    fn state(&self, cx: &gpui::App) -> ActionState {
        self.model.read(cx).core().action(self.kind()).clone()
    }

    /// Closes the dialog once the deletion has gone through, or when the message is already gone.
    fn settle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.waiting {
            return;
        }
        match self.state(cx) {
            ActionState::InFlight => self.started = true,
            ActionState::Idle if self.started => self.finish(window, cx),
            ActionState::Failed(_) => {
                self.waiting = false;
                self.started = false;
            }
            ActionState::Idle => {}
        }
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        if self.waiting {
            return;
        }
        self.waiting = true;
        self.started = false;
        let (conversation, message) = (self.conversation, self.message);
        self.model.update(cx, |model, cx| {
            model.dispatch(
                Input::DeleteMessage {
                    conversation,
                    message,
                },
                cx,
            );
        });
        // A deletion that settled synchronously has already been reported.
        if self.state(cx) == ActionState::Idle {
            self.started = true;
        }
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.waiting && self.state(cx) == ActionState::InFlight {
            return;
        }
        self.finish(window, cx);
    }

    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let kind = self.kind();
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::DismissAction(kind), cx)
        });
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }
}

/// The first part of `text`, on one line.
fn excerpt(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= PREVIEW_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(PREVIEW_CHARS).collect();
    format!("{}…", cut.trim_end())
}

impl Render for DeleteMessageDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let close_to = this.clone();
        let cancel_to = this.clone();
        let delete_to = this;
        let state = self.state(cx);
        let working = self.waiting && state == ActionState::InFlight;
        let error = match &state {
            ActionState::Failed(message) => Some(message.clone()),
            _ => None,
        };

        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_size(theme.type_scale.body)
                    .text_color(colors.text)
                    .child("This message will be deleted for everyone in the conversation. This can't be undone."),
            )
            .when(self.replies > 0, |body| {
                body.child(
                    div()
                        .debug_selector(|| "delete-keeps-replies".into())
                        .text_size(theme.type_scale.small)
                        .text_color(colors.text_muted)
                        .child(format!(
                            "It has {} {}. {} stay, and the message will show as deleted.",
                            self.replies,
                            if self.replies == 1 { "reply" } else { "replies" },
                            if self.replies == 1 { "It" } else { "They" },
                        )),
                )
            })
            .when(!self.preview.is_empty(), |body| {
                body.child(
                    div()
                        .debug_selector(|| "delete-preview".into())
                        .max_h(px(96.))
                        .overflow_hidden()
                        .px_3()
                        .py_2()
                        .rounded(theme.radii.medium)
                        .bg(colors.code_surface)
                        .text_size(theme.type_scale.small)
                        .text_color(colors.text_muted)
                        .child(self.preview.clone()),
                )
            })
            .children(error.map(|message| {
                div()
                    .debug_selector(|| "delete-error".into())
                    .text_size(theme.type_scale.small)
                    .text_color(colors.danger)
                    .child(format!("Couldn't delete the message: {message}"))
            }))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        &theme,
                        "delete-message-cancel",
                        "Cancel",
                        ButtonKind::Secondary,
                        !working,
                        &self.cancel_focus,
                        move |window, cx| {
                            cancel_to
                                .update(cx, |dialog, cx| dialog.dismiss(window, cx))
                                .ok();
                        },
                    ))
                    .child(form::button(
                        &theme,
                        "delete-message-confirm",
                        if working { "Deleting…" } else { "Delete message" },
                        ButtonKind::Danger,
                        !working,
                        &self.delete_focus,
                        move |_, cx| {
                            delete_to.update(cx, |dialog, cx| dialog.delete(cx)).ok();
                        },
                    )),
            )
            .into_any_element();

        dialog_overlay(
            &theme,
            "Delete message",
            460.,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_excerpt_is_one_line_and_bounded() {
        assert_eq!(excerpt("one\n\ntwo   three"), "one two three");
        let long = "word ".repeat(200);
        let shown = excerpt(&long);
        assert!(shown.chars().count() <= PREVIEW_CHARS + 1);
        assert!(shown.ends_with('…'));
    }
}
