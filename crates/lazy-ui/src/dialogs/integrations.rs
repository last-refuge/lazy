//! Incoming webhooks for a channel, in its Details dialog: who may post here as an app, adding one,
//! and removing one. For admins and owners; the section is not drawn for anyone else.
//!
//! # The address is shown once
//!
//! Creating a webhook returns the address to post to, with a secret in it that the server cannot
//! show again. It appears in a read-only field with a Copy button and a plain note. The core holds
//! it only until [`Input::DismissCreatedWebhook`], which this section sends the moment the person
//! leaves that view (Done, Escape, or closing the dialog), and the field's text is erased with it.
//! The address is never written anywhere else: not a log, not the draft, not the content cache.
//!
//! # Keyboard
//!
//! One tab stop per control, in order: in the list, each webhook's Delete and then Add incoming
//! webhook; adding: the name, Cancel, Create; the new address: the address, Copy, Done; deleting:
//! Cancel (first), Delete webhook. Enter in the name creates. Escape steps back one view (and
//! does nothing while an input method is composing); from the list it is the dialog's.

use std::collections::HashMap;

use gpui::{
    AnyElement, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    IntoElement, ParentElement, Render, ScrollHandle, Styled, Subscription, Window, div,
    prelude::*, px,
};
use jiff::Zoned;
use jiff::tz::TimeZone;
use lazy_client_core::{ActionKind, ActionState, Input};
use lazy_domain::{ConversationId, MAX_WEBHOOKS_PER_CONVERSATION, WebhookId};

use super::single_line_field;
use crate::app_model::{AppEvent, AppModel};
use crate::form::{self, ButtonKind};
use crate::states;
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::Theme;
use crate::time_fmt::{ago_label, to_zoned};

/// What the section asks of the dialog around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationsEvent {
    /// Tab or Shift+Tab was pressed in one of the section's text fields.
    Tab { backwards: bool },
}

/// Which view of the section is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    List,
    Adding,
    /// The new address, from the core's `created_webhook`.
    Created,
    ConfirmDelete(WebhookId),
}

pub struct IntegrationsSection {
    model: Entity<AppModel>,
    conversation: ConversationId,
    view: View,
    name: Entity<TextInput>,
    /// Holds the new address while it is shown, read-only; emptied when it is dismissed.
    address: Entity<TextInput>,
    /// The creation or deletion in progress, and whether it has been seen starting.
    waiting: Option<(ActionKind, bool)>,
    /// "Copied" is showing on the Copy button.
    copied: bool,
    add_focus: FocusHandle,
    cancel_focus: FocusHandle,
    create_focus: FocusHandle,
    copy_focus: FocusHandle,
    done_focus: FocusHandle,
    confirm_cancel_focus: FocusHandle,
    confirm_delete_focus: FocusHandle,
    retry_focus: FocusHandle,
    delete_focus: HashMap<WebhookId, FocusHandle>,
    /// Scrolls the list of webhooks, which shows a few rows and no more.
    list_scroll: ScrollHandle,
    /// The row the list was last scrolled to because its Delete took the keyboard.
    scrolled_for: Option<WebhookId>,
    zone: TimeZone,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<IntegrationsEvent> for IntegrationsSection {}

impl IntegrationsSection {
    pub fn new(
        model: Entity<AppModel>,
        conversation: ConversationId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = single_line_field("e.g. Deploy bot", "Webhook name", 64, cx);
        let address = single_line_field("", "New webhook address", 2048, cx);
        address.update(cx, |input, cx| input.set_enabled(false, cx));
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &name,
            window,
            |this, _input, event: &TextInputEvent, _window, cx| match event {
                TextInputEvent::Submit => this.create(cx),
                TextInputEvent::Tab { backwards } => cx.emit(IntegrationsEvent::Tab {
                    backwards: *backwards,
                }),
                TextInputEvent::Changed => cx.notify(),
                TextInputEvent::UpWhenEmpty
                | TextInputEvent::PasteAttachments
                | TextInputEvent::Suggestion(_) => {}
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &address,
            window,
            |_this, _input, event: &TextInputEvent, _window, cx| {
                if let TextInputEvent::Tab { backwards } = event {
                    cx.emit(IntegrationsEvent::Tab {
                        backwards: *backwards,
                    });
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &model,
            window,
            |this, _model, _event: &AppEvent, window, cx| {
                this.follow_core(window, cx);
                cx.notify();
            },
        ));
        // An address left over from an earlier view is forgotten, and what is already on the
        // server is read.
        model.update(cx, |model, cx| {
            if model.core().created_webhook().is_some() {
                model.dispatch(Input::DismissCreatedWebhook, cx);
            }
            model.dispatch(Input::LoadWebhooks(conversation), cx)
        });
        Self {
            model,
            conversation,
            view: View::List,
            name,
            address,
            waiting: None,
            copied: false,
            add_focus: cx.focus_handle().tab_stop(true),
            cancel_focus: cx.focus_handle().tab_stop(true),
            create_focus: cx.focus_handle().tab_stop(true),
            copy_focus: cx.focus_handle().tab_stop(true),
            done_focus: cx.focus_handle().tab_stop(true),
            confirm_cancel_focus: cx.focus_handle().tab_stop(true),
            confirm_delete_focus: cx.focus_handle().tab_stop(true),
            retry_focus: cx.focus_handle().tab_stop(true),
            delete_focus: HashMap::new(),
            list_scroll: ScrollHandle::new(),
            scrolled_for: None,
            zone: TimeZone::system(),
            _subscriptions: subscriptions,
        }
    }

    /// Whether the section is showing its list, as opposed to adding, an address, or a deletion.
    pub fn showing_list(&self) -> bool {
        self.view == View::List
    }

    /// Whether the name box is mid-composition, which makes Escape the input method's.
    pub fn composing(&self, cx: &gpui::App) -> bool {
        self.name.read(cx).is_composing()
    }

    /// The name box, for the dialog's composition check and for tests.
    pub fn name_input(&self) -> &Entity<TextInput> {
        &self.name
    }

    /// The box that holds the new address while it is shown, for tests.
    pub fn address_input(&self) -> &Entity<TextInput> {
        &self.address
    }

    fn action(&self, kind: ActionKind, cx: &gpui::App) -> ActionState {
        self.model.read(cx).core().action(kind).clone()
    }

    /// Follows what the core did with a creation or a deletion.
    fn follow_core(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The address went away under the view that shows it (its webhook was deleted, or the
        // channel was lost): the field is emptied with it.
        if self.view == View::Created && self.model.read(cx).core().created_webhook().is_none() {
            self.finish_created(window, cx);
            return;
        }
        // A new address arrived: show it, once.
        if self.view == View::Adding || self.view == View::List {
            let ready = self
                .model
                .read(cx)
                .core()
                .created_webhook()
                .is_some_and(|created| created.webhook.conversation_id == self.conversation);
            if ready {
                self.show_created(window, cx);
            }
        }
        let Some((kind, started)) = self.waiting else {
            return;
        };
        match self.action(kind, cx) {
            ActionState::InFlight => self.waiting = Some((kind, true)),
            ActionState::Idle if started => {
                self.waiting = None;
                if matches!(kind, ActionKind::DeleteWebhook(_)) {
                    self.back_to_list(window, cx);
                }
            }
            // Failed: the message shows where the person is, and they can try again.
            ActionState::Failed(_) => self.waiting = None,
            ActionState::Idle => {}
        }
    }

    // ----- Moving between views -----

    fn back_to_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.view = View::List;
        window.focus(&self.add_focus, cx);
        cx.notify();
    }

    /// Opens the form for a new webhook's name.
    pub fn start_adding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.dispatch(
                Input::DismissAction(ActionKind::CreateWebhook(self.conversation)),
                cx,
            )
        });
        self.name.update(cx, |input, cx| input.set_text("", cx));
        self.view = View::Adding;
        let handle = self.name.focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    /// Puts the new address in the read-only box and moves to the view that shows it.
    fn show_created(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = self
            .model
            .read(cx)
            .core()
            .created_webhook()
            .map(|created| created.url.as_str().to_owned());
        let Some(url) = url else {
            return;
        };
        // The caret goes to the start so the field shows where the address begins, not its end.
        self.address.update(cx, |input, cx| {
            input.set_text(&url, cx);
            input.set_selection(0, 0, cx);
        });
        self.copied = false;
        self.view = View::Created;
        window.focus(&self.copy_focus, cx);
        cx.notify();
    }

    /// Ends the "new address" view: the core forgets the address and the box is emptied.
    fn finish_created(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.forget_address(cx);
        self.back_to_list(window, cx);
    }

    /// The address leaves memory: the core's copy and the box's text.
    fn forget_address(&mut self, cx: &mut Context<Self>) {
        self.address.update(cx, |input, cx| input.set_text("", cx));
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::DismissCreatedWebhook, cx)
        });
        self.copied = false;
    }

    /// The dialog is closing: nothing of the section may outlive it.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.view == View::Created || self.model.read(cx).core().created_webhook().is_some() {
            self.forget_address(cx);
        }
        let conversation = self.conversation;
        self.model.update(cx, |model, cx| {
            for kind in [
                ActionKind::CreateWebhook(conversation),
                ActionKind::LoadWebhooks(conversation),
            ] {
                model.dispatch(Input::DismissAction(kind), cx);
            }
        });
    }

    /// Escape: back one view. Returns whether it did anything (from the list it does not, and the
    /// dialog closes instead).
    pub fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.composing(cx) {
            return true;
        }
        match self.view {
            View::List => false,
            View::Adding => {
                self.back_to_list(window, cx);
                true
            }
            View::Created => {
                self.finish_created(window, cx);
                true
            }
            View::ConfirmDelete(id) => {
                self.view = View::List;
                self.model.update(cx, |model, cx| {
                    model.dispatch(Input::DismissAction(ActionKind::DeleteWebhook(id)), cx)
                });
                let handle = self.delete_handle(id, cx);
                window.focus(&handle, cx);
                cx.notify();
                true
            }
        }
    }

    /// The tab stop of a webhook's Delete button.
    fn delete_handle(&mut self, id: WebhookId, cx: &mut Context<Self>) -> FocusHandle {
        self.delete_focus
            .entry(id)
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone()
    }

    // ----- Acting -----

    fn create(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text().trim().to_owned();
        let kind = ActionKind::CreateWebhook(self.conversation);
        if name.is_empty() || self.action(kind, cx) == ActionState::InFlight {
            return;
        }
        let conversation = self.conversation;
        self.waiting = Some((kind, false));
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::CreateWebhook { conversation, name }, cx)
        });
    }

    fn delete(&mut self, id: WebhookId, cx: &mut Context<Self>) {
        let kind = ActionKind::DeleteWebhook(id);
        if self.action(kind, cx) == ActionState::InFlight {
            return;
        }
        self.waiting = Some((kind, false));
        self.model
            .update(cx, |model, cx| model.dispatch(Input::DeleteWebhook(id), cx));
    }

    fn copy_address(&mut self, cx: &mut Context<Self>) {
        let url = self
            .model
            .read(cx)
            .core()
            .created_webhook()
            .map(|created| created.url.as_str().to_owned());
        if let Some(url) = url {
            cx.write_to_clipboard(ClipboardItem::new_string(url));
            self.copied = true;
            cx.notify();
        }
    }

    // ----- Drawing -----

    fn heading(&self, theme: &Theme) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .child(
                div()
                    .text_size(theme.type_scale.small)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.colors.text_muted)
                    .child("Integrations"),
            )
            .child(
                div()
                    .text_size(theme.type_scale.small)
                    .text_color(theme.colors.text_faint)
                    .child("Incoming webhooks let a tool post here as an app."),
            )
            .into_any_element()
    }

    fn error_line(&self, theme: &Theme, message: String) -> AnyElement {
        div()
            .debug_selector(|| "integrations-error".into())
            .text_size(theme.type_scale.small)
            .text_color(theme.colors.danger)
            .child(message)
            .into_any_element()
    }

    fn list_view(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let (webhooks, archived, loading, load_error) = {
            let core = self.model.read(cx).core();
            (
                core.webhooks(self.conversation).to_vec(),
                core.conversation(self.conversation)
                    .is_some_and(|c| c.archived),
                core.action(ActionKind::LoadWebhooks(self.conversation)) == &ActionState::InFlight,
                match core.action(ActionKind::LoadWebhooks(self.conversation)) {
                    ActionState::Failed(message) => Some(message.clone()),
                    _ => None,
                },
            )
        };
        let failed = load_error.is_some();
        let now = Zoned::now();
        let full = webhooks.len() >= MAX_WEBHOOKS_PER_CONVERSATION;
        // A few rows show at once; the rest scroll, so a full channel does not push the dialog
        // past the window.
        let mut list = div()
            .id("webhook-list")
            .debug_selector(|| "webhook-list".into())
            .max_h(px(150.))
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            .flex()
            .flex_col()
            .gap_1();
        for (index, webhook) in webhooks.iter().enumerate() {
            let id = webhook.id;
            let focus = self.delete_handle(id, cx);
            // The list shows only a few rows: when the keyboard reaches a row's Delete, scroll
            // the row into view (once, so the mouse wheel is not fought afterwards).
            if focus.is_focused(window) {
                if self.scrolled_for != Some(id) {
                    self.scrolled_for = Some(id);
                    self.list_scroll.scroll_to_item(index);
                }
            } else if self.scrolled_for == Some(id) {
                self.scrolled_for = None;
            }
            let creator = self
                .model
                .read(cx)
                .core()
                .display_name(webhook.created_by)
                .to_owned();
            let when = ago_label(&to_zoned(webhook.created_at, &self.zone), &now);
            let delete_to = this.clone();
            let name = webhook.name.to_string();
            list = list.child(
                div()
                    .debug_selector(|| "webhook-row".into())
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .rounded(theme.radii.medium)
                    .border_1()
                    .border_color(colors.border)
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
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(colors.text)
                                    .child(name.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.text_muted)
                                    .child(format!("Added by {creator} · {when}")),
                            ),
                    )
                    .child(form::button(
                        theme,
                        format!("webhook-delete-{}", webhooks_index(&webhooks, id)),
                        "Delete…",
                        ButtonKind::Danger,
                        true,
                        &focus,
                        move |window, cx| {
                            delete_to
                                .update(cx, |section, cx| {
                                    section.view = View::ConfirmDelete(id);
                                    window.focus(&section.confirm_cancel_focus, cx);
                                    cx.notify();
                                })
                                .ok();
                        },
                    )),
            );
        }
        let add_to = this.clone();
        let retry_to = this;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(self.heading(theme))
            .when(loading && webhooks.is_empty(), |section| {
                section.child(
                    div()
                        .debug_selector(|| "integrations-loading".into())
                        .text_size(theme.type_scale.small)
                        .text_color(colors.text_muted)
                        .child("Loading integrations…"),
                )
            })
            .children(load_error.map(|message| {
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    // The text wraps in the space the button leaves, rather than pushing it out
                    // of the dialog.
                    .child(div().flex_1().min_w_0().child(self.error_line(
                        theme,
                        states::integrations_failed(&message),
                    )))
                    .child(form::button(
                        theme,
                        "integrations-retry",
                        "Retry",
                        ButtonKind::Secondary,
                        true,
                        &self.retry_focus,
                        move |_, cx| {
                            retry_to
                                .update(cx, |section, cx| {
                                    let conversation = section.conversation;
                                    section.model.update(cx, |model, cx| {
                                        model.dispatch(Input::LoadWebhooks(conversation), cx)
                                    });
                                })
                                .ok();
                        },
                    ))
            }))
            .when(
                !loading && !failed && webhooks.is_empty() && !archived,
                |section| {
                    section.child(
                        div()
                            .debug_selector(|| "integrations-empty".into())
                            .text_size(theme.type_scale.small)
                            .text_color(colors.text_muted)
                            .child("No incoming webhooks yet. Add one to let a tool post here."),
                    )
                },
            )
            .when(archived, |section| {
                section.child(
                    div()
                        .debug_selector(|| "integrations-archived".into())
                        .text_size(theme.type_scale.small)
                        .text_color(colors.text_muted)
                        .child("This channel is archived, so it can't get new integrations."),
                )
            })
            .child(list)
            .when(full && !archived, |section| {
                section.child(
                    div()
                        .debug_selector(|| "integrations-full".into())
                        .text_size(theme.type_scale.small)
                        .text_color(colors.text_muted)
                        .child(format!(
                            "A channel can have at most {MAX_WEBHOOKS_PER_CONVERSATION} incoming webhooks. Delete one to add another."
                        )),
                )
            })
            .child(div().flex().child(form::button(
                theme,
                "webhook-add",
                "Add incoming webhook",
                ButtonKind::Secondary,
                !full && !archived,
                &self.add_focus,
                move |window, cx| {
                    add_to
                        .update(cx, |section, cx| section.start_adding(window, cx))
                        .ok();
                },
            )))
            .into_any_element()
    }

    fn adding_view(
        &self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let cancel_to = this.clone();
        let create_to = this;
        let kind = ActionKind::CreateWebhook(self.conversation);
        let state = self.action(kind, cx);
        let working = state == ActionState::InFlight;
        let error = match &state {
            ActionState::Failed(message) => Some(message.clone()),
            _ => None,
        };
        let ready = !self.name.read(cx).text().trim().is_empty() && !working;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(self.heading(theme))
            .child(form::labeled_field(
                theme,
                "Name of the app",
                &self.name,
                Some("It is shown as the author of what it posts.".into()),
                error.map(Into::into),
                window,
                cx,
            ))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "webhook-cancel",
                        "Cancel",
                        ButtonKind::Secondary,
                        true,
                        &self.cancel_focus,
                        move |window, cx| {
                            cancel_to
                                .update(cx, |section, cx| section.back_to_list(window, cx))
                                .ok();
                        },
                    ))
                    .child(form::button(
                        theme,
                        "webhook-create",
                        if working {
                            "Creating…"
                        } else {
                            "Create webhook"
                        },
                        ButtonKind::Primary,
                        ready,
                        &self.create_focus,
                        move |_, cx| {
                            create_to.update(cx, |section, cx| section.create(cx)).ok();
                        },
                    )),
            )
            .into_any_element()
    }

    fn created_view(
        &self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let copy_to = this.clone();
        let done_to = this;
        let name = self
            .model
            .read(cx)
            .core()
            .created_webhook()
            .map(|created| created.webhook.name.to_string())
            .unwrap_or_default();
        let focused = self.address.read(cx).is_focused(window);
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(self.heading(theme))
            .child(
                div()
                    .debug_selector(|| "webhook-note".into())
                    .text_size(theme.type_scale.body)
                    .text_color(colors.text)
                    .child(format!(
                        "This address lets anyone post as {name}. Copy it now: it will not be shown again."
                    )),
            )
            .child(
                div()
                    .debug_selector(|| "webhook-address".into())
                    .px_3()
                    .py_2()
                    .rounded(theme.radii.medium)
                    .bg(colors.surface_raised)
                    .border_1()
                    .border_color(if focused {
                        colors.focus_ring
                    } else {
                        colors.border_strong
                    })
                    .child(self.address.clone()),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "webhook-copy",
                        if self.copied { "Copied" } else { "Copy address" },
                        ButtonKind::Secondary,
                        true,
                        &self.copy_focus,
                        move |_, cx| {
                            copy_to
                                .update(cx, |section, cx| section.copy_address(cx))
                                .ok();
                        },
                    ))
                    .child(form::button(
                        theme,
                        "webhook-done",
                        "Done",
                        ButtonKind::Primary,
                        true,
                        &self.done_focus,
                        move |window, cx| {
                            done_to
                                .update(cx, |section, cx| section.finish_created(window, cx))
                                .ok();
                        },
                    )),
            )
            .into_any_element()
    }

    fn confirm_view(&self, id: WebhookId, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let cancel_to = this.clone();
        let delete_to = this;
        let name = self
            .model
            .read(cx)
            .core()
            .webhooks(self.conversation)
            .iter()
            .find(|webhook| webhook.id == id)
            .map(|webhook| webhook.name.to_string())
            .unwrap_or_else(|| "this webhook".to_owned());
        let state = self.action(ActionKind::DeleteWebhook(id), cx);
        let working = state == ActionState::InFlight;
        let error = match &state {
            ActionState::Failed(message) => Some(message.clone()),
            _ => None,
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(self.heading(theme))
            .child(
                div()
                    .text_size(theme.type_scale.body)
                    .text_color(colors.text)
                    .child(format!(
                        "Delete {name}? Anything still posting to its address stops working, and the app leaves the channel. What it already posted stays."
                    )),
            )
            .children(error.map(|message| self.error_line(theme, message)))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "webhook-delete-cancel",
                        "Cancel",
                        ButtonKind::Secondary,
                        !working,
                        &self.confirm_cancel_focus,
                        move |window, cx| {
                            cancel_to.update(cx, |section, cx| { section.back(window, cx); }).ok();
                        },
                    ))
                    .child(form::button(
                        theme,
                        "webhook-delete-confirm",
                        if working { "Deleting…" } else { "Delete webhook" },
                        ButtonKind::Danger,
                        !working,
                        &self.confirm_delete_focus,
                        move |_, cx| {
                            delete_to
                                .update(cx, |section, cx| section.delete(id, cx))
                                .ok();
                        },
                    )),
            )
            .into_any_element()
    }
}

/// The number of `id` among `webhooks`, for naming each row's Delete button.
fn webhooks_index(webhooks: &[lazy_protocol::Webhook], id: WebhookId) -> usize {
    webhooks.iter().position(|w| w.id == id).unwrap_or(0)
}

impl Render for IntegrationsSection {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        div()
            .debug_selector(|| "integrations".into())
            .pt_3()
            .border_t_1()
            .border_color(theme.colors.border)
            .child(match self.view {
                View::List => self.list_view(&theme, window, cx),
                View::Adding => self.adding_view(&theme, window, cx),
                View::Created => self.created_view(&theme, window, cx),
                View::ConfirmDelete(id) => self.confirm_view(id, &theme, cx),
            })
    }
}
