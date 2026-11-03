//! Settings, Password and security: who the person is signed in as, and changing the password.
//!
//! The three fields are masked. The core checks the new password before anything is sent
//! (`Input::ChangePassword`); the server checks the current one and signs out the person's other
//! sessions. When the change goes through, the fields empty and a line says so.

use gpui::{
    AnyElement, Context, FontWeight, IntoElement, ParentElement, Role, Styled, Window, div,
    prelude::*,
};
use lazy_client_core::{ActionKind, ActionState, Input};

use super::PreferencesDialog;
use crate::form::{self, ButtonKind};
use crate::states;
use crate::theme::Theme;

impl PreferencesDialog {
    /// Sends the three fields as a password change.
    pub(super) fn change_password(&mut self, cx: &mut Context<Self>) {
        if self
            .model
            .read(cx)
            .core()
            .action(ActionKind::ChangePassword)
            == &ActionState::InFlight
        {
            return;
        }
        let text = |input: &gpui::Entity<crate::text_input::TextInput>, cx: &Context<Self>| {
            input.read(cx).text().to_string()
        };
        let input = Input::ChangePassword {
            current: text(&self.current_password, cx),
            new: text(&self.new_password, cx),
            confirm: text(&self.confirm_password, cx),
        };
        self.password_changed = false;
        self.password_sent = true;
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.notify();
    }

    /// Follows the change through: once the action is no longer in flight, it either failed
    /// (the action says why) or went through (the fields empty and a line says so).
    pub(super) fn password_outcome(&mut self, cx: &mut Context<Self>) {
        if !self.password_sent {
            return;
        }
        match self
            .model
            .read(cx)
            .core()
            .action(ActionKind::ChangePassword)
        {
            ActionState::InFlight => {}
            ActionState::Failed(_) => self.password_sent = false,
            ActionState::Idle => {
                self.password_sent = false;
                self.password_changed = true;
                for input in [
                    &self.current_password,
                    &self.new_password,
                    &self.confirm_password,
                ] {
                    input.update(cx, |input, cx| input.set_text("", cx));
                }
            }
        }
    }

    /// Whether the three password fields are empty, for tests.
    pub fn password_fields_empty(&self, cx: &gpui::App) -> bool {
        [
            &self.current_password,
            &self.new_password,
            &self.confirm_password,
        ]
        .iter()
        .all(|input| input.read(cx).text().is_empty())
    }

    pub(super) fn security_section(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let (state, username, server) = {
            let core = self.model.read(cx).core();
            (
                core.action(ActionKind::ChangePassword).clone(),
                core.me()
                    .map(|me| me.username.as_str().to_owned())
                    .unwrap_or_default(),
                core.server().url.clone(),
            )
        };
        let busy = state == ActionState::InFlight;
        let this = cx.entity().downgrade();
        let field = |label: &'static str, input| {
            form::labeled_field(theme, label, input, None, None, window, cx)
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .debug_selector(|| "prefs-signed-in-as".into())
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .text_size(theme.type_scale.small)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.text_muted)
                            .child("Signed in as"),
                    )
                    .child(
                        div()
                            .text_size(theme.type_scale.body)
                            .text_color(colors.text)
                            .child(states::signed_in_as(&username, &server)),
                    ),
            )
            .child(
                div()
                    .pt_2()
                    .text_size(theme.type_scale.body)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.text)
                    .child("Change your password"),
            )
            .child(
                div()
                    .debug_selector(|| "prefs-current-password".into())
                    .child(field("Current password", &self.current_password)),
            )
            .child(
                div()
                    .debug_selector(|| "prefs-new-password".into())
                    .child(field("New password", &self.new_password)),
            )
            .child(
                div()
                    .debug_selector(|| "prefs-confirm-password".into())
                    .child(field("Confirm new password", &self.confirm_password)),
            )
            .child(
                div()
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_faint)
                    .child(states::PASSWORD_CHANGE_NOTE),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(form::button(
                        theme,
                        "prefs-change-password",
                        if busy {
                            "Changing…"
                        } else {
                            "Change password"
                        },
                        ButtonKind::Primary,
                        !busy,
                        &self.change_password_focus,
                        move |_, cx| {
                            this.update(cx, |dialog, cx| dialog.change_password(cx))
                                .ok();
                        },
                    ))
                    .children(match &state {
                        ActionState::Failed(message) => Some(
                            div()
                                .id("prefs-password-failed")
                                .debug_selector(|| "prefs-password-failed".into())
                                .role(Role::Alert)
                                .flex_1()
                                .min_w_0()
                                .text_size(theme.type_scale.small)
                                .text_color(colors.danger)
                                .child(message.clone())
                                .into_any_element(),
                        ),
                        _ if self.password_changed => Some(
                            div()
                                .id("prefs-password-changed")
                                .debug_selector(|| "prefs-password-changed".into())
                                .role(Role::Status)
                                .flex_1()
                                .min_w_0()
                                .text_size(theme.type_scale.small)
                                .text_color(colors.success)
                                .child(states::PASSWORD_CHANGED)
                                .into_any_element(),
                        ),
                        _ => None,
                    }),
            )
            .into_any_element()
    }
}
