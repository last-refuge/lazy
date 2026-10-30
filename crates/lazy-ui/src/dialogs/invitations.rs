//! Invitations (admins and owners): the invitations of the workspace with where each stands, a
//! form that makes a new one, the one-time view of a new code, and Revoke (D22).
//!
//! The server keeps only a hash of a code and shows the code once, in the answer that creates
//! the invitation; the core holds it until Done (or until the dialog closes) and never writes it
//! anywhere. Here it appears in exactly one place, a selectable monospace field labelled
//! "Invitation code": not in a tooltip, a title, or the label of any other element.
//!
//! # Keyboard
//!
//! Tab order: with a new code showing, the code, Copy, Copy message, and Done; then how long the invitation
//! lasts and how many times it can be used (Left and Right choose), Create invitation, each
//! active invitation's Revoke, and Try again when the list failed. Revoke asks first: Enter on
//! "Revoke" confirms, Escape keeps the invitation. Escape otherwise closes the dialog.

use std::collections::HashMap;

use gpui::{
    ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    ParentElement, Render, Role, Styled, Subscription, Window, div, prelude::*, px,
};
use jiff::tz::TimeZone;
use lazy_client_core::{ActionKind, ActionState, Input, LoadStatus};
use lazy_domain::{InvitationId, Timestamp};
use lazy_protocol::{Invitation, InvitationState};

use super::any_composing;
use crate::app_model::{AppEvent, AppModel};
use crate::fonts;
use crate::form::{self, ButtonKind};
use crate::modal::{DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, dialog_overlay};
use crate::states;
use crate::text_input::{TextInput, TextInputEvent, TextInputOptions};
use crate::theme::Theme;
use crate::time_fmt::{ago_label, to_zoned};
use crate::workspace_form::{self, Expiry, Uses};

/// A new invitation's code as it is shown and sent, in groups of four, which is how it is read
/// aloud, typed, and told apart. The one place a code is read out of the core: the invitations
/// dialog and the owner's first-run screen both come here.
pub(crate) fn shown_code(created: &lazy_client_core::CreatedInvite) -> String {
    lazy_domain::workspace::format_invite_code(created.code.expose())
}

pub struct InvitationsDialog {
    model: Entity<AppModel>,
    trap: ModalTrap,
    expiry: Expiry,
    uses: Uses,
    /// The selectable field a new code is shown in, made when the code arrives.
    code_field: Option<Entity<TextInput>>,
    /// The code is shown in `code_field`; edits to it are undone.
    code_text: Option<String>,
    copied: bool,
    /// An invitation being revoked, waiting for the person's yes.
    confirming: Option<InvitationId>,
    zone: TimeZone,
    expiry_focus: FocusHandle,
    uses_focus: FocusHandle,
    create_focus: FocusHandle,
    copy_focus: FocusHandle,
    message_focus: FocusHandle,
    done_focus: FocusHandle,
    retry_focus: FocusHandle,
    confirm_yes_focus: FocusHandle,
    confirm_no_focus: FocusHandle,
    revoke_focus: HashMap<InvitationId, FocusHandle>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for InvitationsDialog {}

impl InvitationsDialog {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![cx.subscribe_in(
            &model,
            window,
            |this, _model, _event: &AppEvent, window, cx| this.sync(window, cx),
        )];
        model.update(cx, |model, cx| model.dispatch(Input::OpenInvitations, cx));
        let mut trap = ModalTrap::new(cx);
        let initial = cx.focus_handle().tab_stop(true);
        trap.open(&initial, window, cx);
        let expiry_focus = initial;
        Self {
            model,
            trap,
            expiry: Expiry::DEFAULT,
            uses: Uses::DEFAULT,
            code_field: None,
            code_text: None,
            copied: false,
            confirming: None,
            zone: TimeZone::system(),
            expiry_focus,
            uses_focus: cx.focus_handle().tab_stop(true),
            create_focus: cx.focus_handle().tab_stop(true),
            copy_focus: cx.focus_handle().tab_stop(true),
            message_focus: cx.focus_handle().tab_stop(true),
            done_focus: cx.focus_handle().tab_stop(true),
            retry_focus: cx.focus_handle().tab_stop(true),
            confirm_yes_focus: cx.focus_handle().tab_stop(true),
            confirm_no_focus: cx.focus_handle().tab_stop(true),
            revoke_focus: HashMap::new(),
            _subscriptions: subscriptions,
        }
    }

    // ----- For tests -----

    /// Which tab stop has the keyboard.
    pub fn stop(&self, window: &Window, cx: &gpui::App) -> &'static str {
        if self
            .code_field
            .as_ref()
            .is_some_and(|field| field.read(cx).is_focused(window))
        {
            "code"
        } else if self.copy_focus.is_focused(window) {
            "copy"
        } else if self.message_focus.is_focused(window) {
            "copy-message"
        } else if self.done_focus.is_focused(window) {
            "done"
        } else if self.expiry_focus.is_focused(window) {
            "expiry"
        } else if self.uses_focus.is_focused(window) {
            "uses"
        } else if self.create_focus.is_focused(window) {
            "create"
        } else if self.retry_focus.is_focused(window) {
            "retry"
        } else if self.confirm_yes_focus.is_focused(window) {
            "confirm"
        } else if self.confirm_no_focus.is_focused(window) {
            "keep"
        } else if self
            .revoke_focus
            .values()
            .any(|focus| focus.is_focused(window))
        {
            "revoke"
        } else {
            "elsewhere"
        }
    }

    /// The choices the form would create an invitation with.
    pub fn choices(&self) -> (Expiry, Uses) {
        (self.expiry, self.uses)
    }

    // ----- Following the core -----

    /// Shows a new code in its field the moment it arrives, and forgets it when it is gone.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Demoted while open: there is nothing left to offer.
        if !self.model.read(cx).core().can_manage_workspace() {
            self.finish(window, cx);
            return;
        }
        let code = self
            .model
            .read(cx)
            .core()
            .invitations()
            .created
            .as_ref()
            // In groups of four, which is how it is read aloud, typed, and told apart.
            .map(shown_code);
        match (&code, &self.code_text) {
            (Some(code), None) => {
                let field = cx.new(|cx| {
                    TextInput::new(
                        TextInputOptions::new("")
                            .accessible_label("Invitation code")
                            .single_line(true)
                            .max_bytes(128),
                        cx,
                    )
                });
                field.update(cx, |input, cx| input.set_text(code, cx));
                // The field is for selecting and copying: typing in it changes nothing.
                let restore = cx.subscribe_in(
                    &field,
                    window,
                    |this, input, event: &TextInputEvent, window, cx| match event {
                        TextInputEvent::Changed => {
                            if let Some(code) = this.code_text.clone() {
                                input.update(cx, |input, cx| input.set_text(&code, cx));
                            }
                        }
                        TextInputEvent::Tab { backwards } => this.trap.tab(*backwards, window, cx),
                        _ => {}
                    },
                );
                self._subscriptions.push(restore);
                let focus = field.focus_handle(cx);
                self.code_field = Some(field);
                self.code_text = Some(code.clone());
                self.copied = false;
                // The code is the first thing to read and copy.
                window.focus(&focus, cx);
            }
            (None, Some(_)) => {
                self.code_field = None;
                self.code_text = None;
                self.copied = false;
                window.focus(&self.expiry_focus, cx);
            }
            _ => {}
        }
        if let Some(id) = self.confirming
            && self
                .model
                .read(cx)
                .core()
                .invitations()
                .items
                .iter()
                .all(|item| item.id != id || item.state != InvitationState::Active)
        {
            // It was revoked (here or elsewhere), or lapsed: nothing left to ask.
            self.confirming = None;
            window.focus(&self.expiry_focus, cx);
        }
        cx.notify();
    }

    fn dispatch(&self, input: Input, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
    }

    // ----- Acting -----

    fn create(&mut self, cx: &mut Context<Self>) {
        if self
            .model
            .read(cx)
            .core()
            .action(ActionKind::CreateInvitation)
            == &ActionState::InFlight
        {
            return;
        }
        self.dispatch(
            Input::CreateInvitation {
                expires_in_secs: Some(self.expiry.secs()),
                max_uses: self.uses.max(),
            },
            cx,
        );
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(code) = &self.code_text {
            cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));
            self.copied = true;
            cx.notify();
        }
    }

    /// Copies the whole message to send: where to get Lazy, the server, the code, and for how long
    /// and how many it works.
    fn copy_message(&mut self, cx: &mut Context<Self>) {
        let Some(code) = &self.code_text else {
            return;
        };
        let (server, created) = {
            let core = self.model.read(cx).core();
            (
                core.server().url.clone(),
                core.invitations().created.clone(),
            )
        };
        let Some(created) = created else {
            return;
        };
        let message = workspace_form::invitation_message(
            &server,
            code,
            &created.invitation,
            Timestamp::now(),
            &self.zone,
        );
        cx.write_to_clipboard(ClipboardItem::new_string(message));
        self.copied = true;
        cx.notify();
    }

    fn done(&mut self, cx: &mut Context<Self>) {
        self.dispatch(Input::DismissCreatedInvitation, cx);
    }

    fn ask_to_revoke(&mut self, id: InvitationId, window: &mut Window, cx: &mut Context<Self>) {
        self.confirming = Some(id);
        window.focus(&self.confirm_no_focus, cx);
        cx.notify();
    }

    fn revoke(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.confirming.take() else {
            return;
        };
        self.dispatch(Input::RevokeInvitation(id), cx);
        window.focus(&self.expiry_focus, cx);
        cx.notify();
    }

    fn keep(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.confirming.take() {
            if let Some(focus) = self.revoke_focus.get(&id) {
                window.focus(focus, cx);
            }
            cx.notify();
        }
    }

    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dispatch(Input::OpenInvitations, cx);
        // The button goes away while it loads: the keyboard stays in the dialog.
        window.focus(&self.expiry_focus, cx);
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirming.is_some() {
            self.keep(window, cx);
            return;
        }
        if let Some(field) = &self.code_field
            && any_composing(&[field], cx)
        {
            return;
        }
        self.finish(window, cx);
    }

    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.code_field = None;
        self.code_text = None;
        let revokes: Vec<InvitationId> = self.revoke_focus.keys().copied().collect();
        self.model.update(cx, |model, cx| {
            // The code goes with the view: it is shown once.
            model.dispatch(Input::CloseInvitations, cx);
            model.dispatch(Input::DismissAction(ActionKind::CreateInvitation), cx);
            for id in revokes {
                model.dispatch(Input::DismissAction(ActionKind::RevokeInvitation(id)), cx);
            }
        });
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }

    // ----- Drawing -----

    fn created_panel(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let colors = theme.colors;
        let field = self.code_field.clone()?;
        let created = self.model.read(cx).core().invitations().created.clone()?;
        let this = cx.entity().downgrade();
        let copy_to = this.clone();
        let message_to = this.clone();
        let done_to = this.clone();
        let now = Timestamp::now();
        let facts = [
            Some(workspace_form::capacity_text(created.invitation.max_uses)),
            workspace_form::expiry_text(&created.invitation, now, &self.zone),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        Some(
            div()
                .id("invite-created")
                .debug_selector(|| "invite-created".into())
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .rounded(theme.radii.medium)
                .bg(colors.accent_surface)
                .border_1()
                .border_color(colors.accent)
                .child(
                    div()
                        .text_size(theme.type_scale.body)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.text)
                        .child("Invitation created"),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .px_3()
                                .py_2()
                                .rounded(theme.radii.medium)
                                .bg(colors.surface_raised)
                                .border_1()
                                .border_color(colors.border_strong)
                                .font_family(fonts::MONO_FAMILY)
                                .text_size(theme.type_scale.body)
                                .child(field),
                        )
                        .child(form::button(
                            theme,
                            "invite-copy",
                            "Copy",
                            ButtonKind::Primary,
                            true,
                            &self.copy_focus,
                            move |_, cx| {
                                copy_to.update(cx, |dialog, cx| dialog.copy(cx)).ok();
                            },
                        )),
                )
                .child(div().flex().child(form::button(
                    theme,
                    "invite-copy-message",
                    "Copy message to send",
                    ButtonKind::Secondary,
                    true,
                    &self.message_focus,
                    move |_, cx| {
                        message_to
                            .update(cx, |dialog, cx| dialog.copy_message(cx))
                            .ok();
                    },
                )))
                .child(
                    div()
                        .id("invite-shown-once")
                        .debug_selector(|| "invite-shown-once".into())
                        .text_size(theme.type_scale.small)
                        .text_color(colors.text)
                        .child(if self.copied {
                            format!("{} {}", states::INVITE_COPIED, states::INVITE_SHOWN_ONCE)
                        } else {
                            states::INVITE_SHOWN_ONCE.to_owned()
                        }),
                )
                .children((!facts.is_empty()).then(|| {
                    div()
                        .text_size(theme.type_scale.small)
                        .text_color(colors.text_muted)
                        .child(facts)
                }))
                .child(div().flex().justify_end().child(form::button(
                    theme,
                    "invite-done",
                    "Done",
                    ButtonKind::Secondary,
                    true,
                    &self.done_focus,
                    move |_, cx| {
                        done_to.update(cx, |dialog, cx| dialog.done(cx)).ok();
                    },
                )))
                .into_any_element(),
        )
    }

    fn create_form(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let expiry_to = this.clone();
        let uses_to = this.clone();
        let create_to = this.clone();
        let state = self
            .model
            .read(cx)
            .core()
            .action(ActionKind::CreateInvitation)
            .clone();
        let creating = state == ActionState::InFlight;
        let notice = self.model.read(cx).core().notice().map(str::to_owned);
        let heading = |text: &'static str| {
            div()
                .text_size(theme.type_scale.small)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.text_muted)
                .child(text)
        };
        let expiry_options: Vec<(&'static str, &'static str)> = Expiry::ALL
            .iter()
            .zip([
                "invite-expiry-hour",
                "invite-expiry-day",
                "invite-expiry-week",
                "invite-expiry-month",
            ])
            .map(|(expiry, selector)| (expiry.label(), selector))
            .collect();
        let uses_options: Vec<(&'static str, &'static str)> = Uses::ALL
            .iter()
            .zip([
                "invite-uses-unlimited",
                "invite-uses-one",
                "invite-uses-five",
                "invite-uses-25",
                "invite-uses-100",
            ])
            .map(|(uses, selector)| (uses.label(), selector))
            .collect();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_size(theme.type_scale.body)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.text)
                    .child("New invitation"),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x_4()
                    .gap_y_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(heading("Expires in"))
                            .child(form::toggle_group(
                                theme,
                                "invite-expiry",
                                "How long the invitation lasts",
                                &expiry_options,
                                Expiry::ALL
                                    .iter()
                                    .position(|e| *e == self.expiry)
                                    .unwrap_or(2),
                                &self.expiry_focus,
                                move |index, _, cx| {
                                    expiry_to
                                        .update(cx, |dialog, cx| {
                                            dialog.expiry = Expiry::ALL[index];
                                            cx.notify();
                                        })
                                        .ok();
                                },
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(heading("Uses"))
                            .child(form::toggle_group(
                                theme,
                                "invite-uses",
                                "How many times it can be used",
                                &uses_options,
                                Uses::ALL.iter().position(|u| *u == self.uses).unwrap_or(0),
                                &self.uses_focus,
                                move |index, _, cx| {
                                    uses_to
                                        .update(cx, |dialog, cx| {
                                            dialog.uses = Uses::ALL[index];
                                            cx.notify();
                                        })
                                        .ok();
                                },
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(form::button(
                        theme,
                        "invite-create",
                        if creating {
                            "Creating…"
                        } else {
                            "Create invitation"
                        },
                        ButtonKind::Primary,
                        !creating,
                        &self.create_focus,
                        move |_, cx| {
                            create_to.update(cx, |dialog, cx| dialog.create(cx)).ok();
                        },
                    ))
                    .children(match &state {
                        ActionState::Failed(message) => Some(
                            div()
                                .id("invite-create-failed")
                                .debug_selector(|| "invite-create-failed".into())
                                .role(Role::Alert)
                                .flex_1()
                                .min_w_0()
                                .text_size(theme.type_scale.small)
                                .text_color(colors.danger)
                                .child(message.clone()),
                        ),
                        _ => None,
                    }),
            )
            .children(notice.map(|notice| {
                div()
                    .id("invite-notice")
                    .debug_selector(|| "invite-notice".into())
                    .text_size(theme.type_scale.small)
                    .text_color(colors.warning)
                    .child(notice)
            }))
            .into_any_element()
    }

    fn row(
        &mut self,
        theme: &Theme,
        index: usize,
        invitation: &Invitation,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let id = invitation.id;
        let now = Timestamp::now();
        let (maker, when) = {
            let core = self.model.read(cx).core();
            (
                core.display_name(invitation.created_by).to_owned(),
                ago_label(
                    &to_zoned(invitation.created_at, &self.zone),
                    &to_zoned(now, &self.zone),
                ),
            )
        };
        let active = invitation.state == InvitationState::Active;
        let facts = [
            Some(workspace_form::uses_text(
                invitation.uses,
                invitation.max_uses,
            )),
            workspace_form::expiry_text(invitation, now, &self.zone),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        let (badge_bg, badge_fg) = match invitation.state {
            InvitationState::Active => (colors.accent_surface, colors.accent),
            InvitationState::Expired | InvitationState::UsedUp => {
                (colors.surface_hover, colors.text_muted)
            }
            InvitationState::Revoked => (colors.danger_surface, colors.danger),
        };
        let confirming = self.confirming == Some(id);
        let focus = self
            .revoke_focus
            .entry(id)
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone();
        let revoke_state = self
            .model
            .read(cx)
            .core()
            .action(ActionKind::RevokeInvitation(id))
            .clone();
        let revoking = revoke_state == ActionState::InFlight;
        let ask_to = this.clone();
        let yes_to = this.clone();
        let no_to = this.clone();
        div()
            .id(("invite-row", index))
            .debug_selector(move || format!("invite-row-{index}"))
            .role(Role::ListItem)
            .aria_label(format!(
                "{} invitation, {}",
                workspace_form::state_label(invitation.state),
                facts
            ))
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .rounded(theme.radii.medium)
            .border_1()
            .border_color(colors.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .debug_selector(move || format!("invite-state-{index}"))
                            .flex_none()
                            .px_1p5()
                            .rounded(theme.radii.small)
                            .bg(badge_bg)
                            .text_color(badge_fg)
                            .text_size(theme.type_scale.small)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(workspace_form::state_label(invitation.state)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(theme.type_scale.body)
                            .text_color(if active {
                                colors.text
                            } else {
                                colors.text_muted
                            })
                            .child(facts.clone()),
                    )
                    .children((active && !confirming).then(|| {
                        form::button(
                            theme,
                            format!("invite-revoke-{index}"),
                            if revoking { "Revoking…" } else { "Revoke" },
                            ButtonKind::Secondary,
                            !revoking,
                            &focus,
                            move |window, cx| {
                                ask_to
                                    .update(cx, |dialog, cx| dialog.ask_to_revoke(id, window, cx))
                                    .ok();
                            },
                        )
                    })),
            )
            .child(
                div()
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_faint)
                    .child(format!("Created by {maker}, {when}")),
            )
            .children(confirming.then(|| {
                div()
                    .id(("invite-confirm", index))
                    .debug_selector(move || format!("invite-confirm-{index}"))
                    .role(Role::Alert)
                    .flex()
                    .flex_col()
                    .gap_2()
                    .pt_1()
                    .child(
                        div()
                            .text_size(theme.type_scale.body)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.text)
                            .child(states::revoke_invitation_question()),
                    )
                    .child(
                        div()
                            .text_size(theme.type_scale.small)
                            .text_color(colors.text_muted)
                            .child(states::REVOKE_INVITATION_DETAIL),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(form::button(
                                theme,
                                "invite-revoke-yes",
                                "Revoke",
                                ButtonKind::Danger,
                                true,
                                &self.confirm_yes_focus,
                                move |window, cx| {
                                    yes_to
                                        .update(cx, |dialog, cx| dialog.revoke(window, cx))
                                        .ok();
                                },
                            ))
                            .child(form::button(
                                theme,
                                "invite-revoke-no",
                                "Keep",
                                ButtonKind::Secondary,
                                true,
                                &self.confirm_no_focus,
                                move |window, cx| {
                                    no_to.update(cx, |dialog, cx| dialog.keep(window, cx)).ok();
                                },
                            )),
                    )
            }))
            .children(match &revoke_state {
                ActionState::Failed(message) => Some(
                    div()
                        .debug_selector(move || format!("invite-revoke-failed-{index}"))
                        .text_size(theme.type_scale.small)
                        .text_color(colors.danger)
                        .child(message.clone()),
                ),
                _ => None,
            })
            .into_any_element()
    }

    fn list(&mut self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = theme.colors;
        let (status, items) = {
            let view = self.model.read(cx).core().invitations();
            (view.status.clone(), view.items.clone())
        };
        let this = cx.entity().downgrade();
        let heading = div()
            .text_size(theme.type_scale.body)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(colors.text)
            .child("Invitations");
        let body: gpui::AnyElement = match (&status, items.is_empty()) {
            (LoadStatus::Idle | LoadStatus::Loading, true) => {
                let bar = |width: f32| {
                    div()
                        .h(px(12.))
                        .w(px(width))
                        .max_w_full()
                        .rounded(theme.radii.small)
                        .bg(colors.border)
                };
                div()
                    .id("invite-loading")
                    .debug_selector(|| "invite-loading".into())
                    .role(Role::Status)
                    .aria_label("Loading invitations")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children((0..3).map(|n| {
                        div()
                            .flex()
                            .flex_col()
                            .gap_1p5()
                            .px_3()
                            .py_2()
                            .rounded(theme.radii.medium)
                            .border_1()
                            .border_color(colors.border)
                            .child(bar(120.0 + n as f32 * 40.0))
                            .child(bar(180.0))
                    }))
                    .into_any_element()
            }
            (LoadStatus::Failed { reason, can_retry }, true) => div()
                .id("invite-failed")
                .debug_selector(|| "invite-failed".into())
                .role(Role::Alert)
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.text)
                        .child(states::INVITATIONS_FAILED_TITLE),
                )
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_color(colors.danger)
                                .child(states::sentence(reason)),
                        )
                        .children(can_retry.then(|| {
                            let retry_to = this.clone();
                            form::button(
                                theme,
                                "invite-retry",
                                states::TRY_AGAIN,
                                ButtonKind::Secondary,
                                true,
                                &self.retry_focus,
                                move |window, cx| {
                                    retry_to
                                        .update(cx, |dialog, cx| dialog.retry(window, cx))
                                        .ok();
                                },
                            )
                        })),
                )
                .into_any_element(),
            (LoadStatus::Ready, true) => div()
                .id("invite-empty")
                .debug_selector(|| "invite-empty".into())
                .text_color(colors.text_muted)
                .text_size(theme.type_scale.body)
                .child(states::INVITATIONS_EMPTY)
                .into_any_element(),
            _ => {
                let rows: Vec<gpui::AnyElement> = items
                    .iter()
                    .enumerate()
                    .map(|(index, invitation)| self.row(theme, index, invitation, cx))
                    .collect();
                div()
                    .id("invite-list")
                    .debug_selector(|| "invite-list".into())
                    .role(Role::List)
                    .aria_label("Invitations")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(rows)
                    .into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(heading)
            .child(body)
            .into_any_element()
    }
}

impl Focusable for InvitationsDialog {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.expiry_focus.clone()
    }
}

impl Render for InvitationsDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let close_to = cx.entity().downgrade();
        let panel = self.created_panel(&theme, cx);
        let form = self.create_form(&theme, cx);
        let list = self.list(&theme, cx);
        let body = div()
            .flex()
            .flex_col()
            .gap_4()
            .children(panel)
            .child(form)
            .child(list)
            .into_any_element();
        dialog_overlay(
            &theme,
            "Invite people",
            640.,
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
