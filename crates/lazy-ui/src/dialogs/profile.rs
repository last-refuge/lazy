//! A person's profile: their name, `@username`, status, role, and when they joined, with a
//! "Message" button (for yourself, "Edit status").
//!
//! It opens at once with what the core already knows about the person (the member list), and
//! fills in the role and the join date when the server's answer arrives.
//!
//! # Keyboard
//!
//! The primary button (Message, or Edit status for yourself) has the keyboard when it opens. Tab
//! order follows the screen from the top: "Try again" when the rest of the profile failed to
//! load, the role control and Deactivate or Reactivate for admins and owners, then the primary
//! button and Close. Escape closes; with a confirmation up (Deactivate, Reactivate), Escape
//! cancels it first.
//!
//! # Administration
//!
//! An admin or owner sees, once the person's role is known, the roles they may give (only those
//! `check_role_change` allows; none when they may not change this person's role) and, when
//! `check_member_action` allows it, Deactivate or Reactivate. Both name the person and what
//! happens before anything is sent. The server decides; its refusal is shown in place.

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, ParentElement,
    Render, Role, Styled, Subscription, Window, div, prelude::*, px,
};
use jiff::tz::TimeZone;
use lazy_client_core::{ActionKind, ActionState, Change, Input, LoadStatus};
use lazy_domain::{UserId, WorkspaceRole};

use crate::app_model::{AppEvent, AppModel};
use crate::form::{self, ButtonKind};
use crate::modal::{DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, dialog_overlay};
use crate::states;
use crate::status_mark::StatusMark;
use crate::theme::Theme;
use crate::time_fmt::to_zoned;
use crate::workspace_form;

/// What the profile asks the shell to do after it has closed itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileRequest {
    /// "Edit status" on the person's own profile: open Preferences.
    EditStatus,
}

pub struct ProfileDialog {
    model: Entity<AppModel>,
    trap: ModalTrap,
    user: UserId,
    primary_focus: FocusHandle,
    retry_focus: FocusHandle,
    close_focus: FocusHandle,
    /// "Message" was pressed: the conversation opening closes the dialog.
    messaging: bool,
    /// Deactivate or Reactivate was pressed and waits for the person's yes.
    confirming: bool,
    role_focus: FocusHandle,
    deactivate_focus: FocusHandle,
    confirm_yes_focus: FocusHandle,
    confirm_no_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

/// What the administration section reads from the core.
struct Admin {
    /// The signed-in person's role now.
    my_role: Option<WorkspaceRole>,
    /// The person shown is deactivated.
    deactivated: bool,
    workspace: String,
    role_state: ActionState,
    activation_state: ActionState,
}

impl EventEmitter<DialogEvent> for ProfileDialog {}
impl EventEmitter<ProfileRequest> for ProfileDialog {}

impl ProfileDialog {
    pub fn new(
        model: Entity<AppModel>,
        user: UserId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = Vec::new();
        subscriptions.push(
            cx.observe(&crate::image_store::ImageStore::global(cx), |_, _, cx| {
                cx.notify()
            }),
        );
        subscriptions.push(cx.subscribe_in(
            &model,
            window,
            |this, _model, event: &AppEvent, window, cx| {
                let AppEvent::Changed(changes) = event;
                // "Message" worked: the conversation is open behind the dialog.
                if changes
                    .iter()
                    .any(|change| matches!(change, Change::ConversationOpened(_)))
                    && this.messaging
                {
                    this.finish(window, cx);
                    return;
                }
                cx.notify();
            },
        ));
        model.update(cx, |model, cx| {
            model.dispatch(Input::OpenProfile { user_id: user }, cx)
        });
        let primary_focus = cx.focus_handle().tab_stop(true);
        let close_focus = cx.focus_handle().tab_stop(true);
        let mut trap = ModalTrap::new(cx);
        let has_primary = {
            let core = model.read(cx).core();
            !core.is_bot(user)
        };
        let initial = if has_primary {
            primary_focus.clone()
        } else {
            close_focus.clone()
        };
        trap.open(&initial, window, cx);
        Self {
            model,
            trap,
            user,
            primary_focus,
            retry_focus: cx.focus_handle().tab_stop(true),
            close_focus,
            messaging: false,
            confirming: false,
            role_focus: cx.focus_handle().tab_stop(true),
            deactivate_focus: cx.focus_handle().tab_stop(true),
            confirm_yes_focus: cx.focus_handle().tab_stop(true),
            confirm_no_focus: cx.focus_handle().tab_stop(true),
            _subscriptions: subscriptions,
        }
    }

    fn is_me(&self, cx: &gpui::App) -> bool {
        self.model
            .read(cx)
            .core()
            .me()
            .is_some_and(|me| me.id == self.user)
    }

    fn message(&mut self, cx: &mut Context<Self>) {
        if self.is_me(cx) {
            return;
        }
        self.messaging = true;
        let user = self.user;
        self.model.update(cx, |model, cx| {
            model.dispatch(
                Input::OpenConversation {
                    participants: vec![user],
                },
                cx,
            )
        });
    }

    /// Closes the dialog and asks the host for Preferences. The request is emitted first: the
    /// host's subscription to it lives in the overlay, which is dropped when it handles the
    /// dismissal (see `DetailsDialog::request`).
    fn edit_status(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ProfileRequest::EditStatus);
        self.finish(window, cx);
    }

    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The button it was on goes away: the keyboard moves to Close, which is always there.
        window.focus(&self.close_focus, cx);
        let user = self.user;
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::OpenProfile { user_id: user }, cx)
        });
    }

    /// Which tab stop has the keyboard, for tests.
    pub fn stop(&self, window: &Window) -> &'static str {
        if self.primary_focus.is_focused(window) {
            "primary"
        } else if self.role_focus.is_focused(window) {
            "role"
        } else if self.deactivate_focus.is_focused(window) {
            "deactivate"
        } else if self.retry_focus.is_focused(window) {
            "retry"
        } else if self.confirm_yes_focus.is_focused(window) {
            "confirm"
        } else if self.confirm_no_focus.is_focused(window) {
            "cancel"
        } else if self.close_focus.is_focused(window) {
            "close"
        } else {
            "elsewhere"
        }
    }

    fn set_role(&mut self, role: WorkspaceRole, cx: &mut Context<Self>) {
        let user = self.user;
        self.model.update(cx, |model, cx| {
            model.dispatch(
                Input::SetMemberRole {
                    user_id: user,
                    role,
                },
                cx,
            )
        });
    }

    fn ask_to_change_activation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirming = true;
        window.focus(&self.confirm_no_focus, cx);
        cx.notify();
    }

    fn change_activation(
        &mut self,
        deactivated: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let user = self.user;
        self.confirming = false;
        self.model.update(cx, |model, cx| {
            model.dispatch(
                Input::SetMemberDeactivated {
                    user_id: user,
                    deactivated,
                },
                cx,
            )
        });
        window.focus(&self.deactivate_focus, cx);
        cx.notify();
    }

    fn cancel_confirmation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirming = false;
        window.focus(&self.deactivate_focus, cx);
        cx.notify();
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirming {
            self.cancel_confirmation(window, cx);
            return;
        }
        self.finish(window, cx);
    }

    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let user = self.user;
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::CloseProfile, cx);
            model.dispatch(Input::DismissAction(ActionKind::OpenConversation), cx);
            model.dispatch(Input::DismissAction(ActionKind::SetMemberRole(user)), cx);
            model.dispatch(
                Input::DismissAction(ActionKind::SetMemberDeactivated(user)),
                cx,
            );
        });
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }
}

impl Focusable for ProfileDialog {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.primary_focus.clone()
    }
}

impl Render for ProfileDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        // Whichever of the member list, the directory and the answer to this profile has them.
        let version = self.model.read(cx).core().avatar_version(self.user);
        crate::avatar_images::request_versions_later(&self.model, vec![(self.user, version)], cx);
        let picture = crate::avatar_images::image_at(
            crate::image_store::ImageStore::global(cx).read(cx),
            self.user,
            version,
        );
        let is_me = self.is_me(cx);
        let (name, username, is_bot, person, load, status, opening, admin) = {
            let core = self.model.read(cx).core();
            let loaded = core
                .profile()
                .filter(|view| view.user_id == self.user)
                .cloned();
            let person = loaded.as_ref().and_then(|view| view.person.clone());
            // What the core knows now, whichever of the member list and the answer has it.
            let user = person
                .as_ref()
                .map(|person| person.user.clone())
                .or_else(|| core.member(self.user).cloned());
            (
                user.as_ref()
                    .map(|user| user.display_name.to_string())
                    .unwrap_or_default(),
                user.as_ref()
                    .map(|user| format!("@{}", user.username))
                    .unwrap_or_default(),
                user.as_ref().is_some_and(|user| user.is_bot),
                person,
                loaded
                    .map(|view| view.status)
                    .unwrap_or(LoadStatus::Loading),
                StatusMark::lookup(core, self.user),
                core.action(ActionKind::OpenConversation).clone(),
                Admin {
                    my_role: core.my_role(),
                    deactivated: user.as_ref().is_some_and(|user| user.deactivated),
                    workspace: core
                        .primary_workspace()
                        .map(|info| info.workspace.name.clone())
                        .unwrap_or_default(),
                    role_state: core.action(ActionKind::SetMemberRole(self.user)).clone(),
                    activation_state: core
                        .action(ActionKind::SetMemberDeactivated(self.user))
                        .clone(),
                },
            )
        };
        let role = person
            .as_ref()
            .and_then(|person| states::role_label(person.role));
        let since = person.as_ref().map(|person| {
            states::member_since(to_zoned(person.joined_at, &TimeZone::system()).date())
        });

        let manager = admin.my_role.filter(|role| role.can_manage_workspace());
        let role_choices: Vec<WorkspaceRole> = match (manager, person.as_ref()) {
            (Some(actor), Some(person)) => workspace_form::role_choices(actor, person.role),
            _ => Vec::new(),
        };
        let can_change_activation = match (manager, person.as_ref()) {
            (Some(actor), Some(person)) => {
                workspace_form::may_deactivate(actor, person.role, is_me)
            }
            _ => false,
        };
        let role_to = this.clone();
        let ask_to = this.clone();
        let yes_to = this.clone();
        let no_to = this.clone();
        let badge = |text: &'static str, selector: &'static str| {
            div()
                .debug_selector(move || selector.into())
                .px_1p5()
                .rounded(theme.radii.small)
                .bg(colors.accent_surface)
                .text_color(colors.accent)
                .text_size(theme.type_scale.small)
                .font_weight(FontWeight::SEMIBOLD)
                .child(text)
        };
        let message_to = this.clone();
        let edit_to = this.clone();
        let retry_to = this.clone();
        let close_to = this.clone();
        let close_button_to = this.clone();
        let body = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(
                        crate::avatar::Avatar::new(
                            self.user.as_uuid().as_u128(),
                            name.clone(),
                            64.,
                        )
                        .text_size(theme.type_scale.heading)
                        .image(picture)
                        .into_div()
                        .debug_selector(|| "profile-avatar".into()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .debug_selector(|| "profile-name".into())
                                            .min_w_0()
                                            .truncate()
                                            .text_size(theme.type_scale.heading)
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(colors.text)
                                            .child(name.clone()),
                                    )
                                    .children(role.map(|role| {
                                        badge(
                                            role,
                                            if role == "Owner" {
                                                "profile-role-owner"
                                            } else {
                                                "profile-role-admin"
                                            },
                                        )
                                    }))
                                    .children(is_bot.then(|| badge("APP", "profile-app")))
                                    .children(admin.deactivated.then(|| {
                                        div()
                                            .id("profile-deactivated")
                                            .debug_selector(|| "profile-deactivated".into())
                                            .px_1p5()
                                            .rounded(theme.radii.small)
                                            .border_1()
                                            .border_color(colors.border_strong)
                                            .text_color(colors.text_muted)
                                            .text_size(theme.type_scale.small)
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .tooltip(|_, cx| {
                                                crate::tooltip::TextTooltip::view(
                                                    states::DEACTIVATED_NOTE,
                                                    cx,
                                                )
                                            })
                                            .child(states::DEACTIVATED)
                                    })),
                            )
                            .child(
                                div()
                                    .debug_selector(|| "profile-username".into())
                                    .text_size(theme.type_scale.body)
                                    .text_color(colors.text_muted)
                                    .child(username),
                            ),
                    ),
            )
            .children(status.map(|mark| {
                div()
                    .debug_selector(|| "profile-status".into())
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded(theme.radii.medium)
                    .bg(colors.surface_hover)
                    .child(crate::status_mark::view(
                        "profile-status-mark",
                        "profile-status-mark".into(),
                        &mark,
                        theme.type_scale.title,
                    ))
                    .child(
                        div()
                            .text_size(theme.type_scale.body)
                            .text_color(colors.text)
                            .child(mark.text.clone()),
                    )
            }))
            .child(match (&since, &load) {
                (Some(since), _) => div()
                    .debug_selector(|| "profile-since".into())
                    .text_size(theme.type_scale.body)
                    .text_color(colors.text_muted)
                    .child(since.clone())
                    .into_any_element(),
                (None, LoadStatus::Failed { reason, can_retry }) => div()
                    .id("profile-failed")
                    .debug_selector(|| "profile-failed".into())
                    .role(Role::Alert)
                    .flex()
                    .items_start()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(theme.type_scale.body)
                            .text_color(colors.danger)
                            .child(if *can_retry {
                                states::profile_failed(reason)
                            } else {
                                states::PROFILE_GONE.to_owned()
                            }),
                    )
                    .children(can_retry.then(|| {
                        form::button(
                            &theme,
                            "profile-retry",
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
                    }))
                    .into_any_element(),
                (None, _) => div()
                    .id("profile-loading")
                    .debug_selector(|| "profile-loading".into())
                    .role(Role::Status)
                    .aria_label("Loading the rest of the profile")
                    .h(px(14.))
                    .w(px(180.))
                    .rounded(theme.radii.small)
                    .bg(colors.border)
                    .into_any_element(),
            })
            .children(match &opening {
                ActionState::Failed(message) => Some(
                    div()
                        .debug_selector(|| "profile-message-failed".into())
                        .text_size(theme.type_scale.small)
                        .text_color(colors.danger)
                        .child(message.clone()),
                ),
                _ => None,
            })
            .children(admin.deactivated.then(|| {
                div()
                    .debug_selector(|| "profile-deactivated-note".into())
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_muted)
                    .child(states::DEACTIVATED_NOTE)
            }))
            .children(
                (!role_choices.is_empty() || can_change_activation).then(|| {
                    let current = person.as_ref().map_or(WorkspaceRole::Member, |p| p.role);
                    let selected = role_choices.iter().position(|r| *r == current).unwrap_or(0);
                    let options: Vec<(&'static str, &'static str)> = role_choices
                        .iter()
                        .map(|role| {
                            (
                                workspace_form::role_name(*role),
                                match role {
                                    WorkspaceRole::Owner => "profile-set-owner",
                                    WorkspaceRole::Admin => "profile-set-admin",
                                    WorkspaceRole::Member => "profile-set-member",
                                },
                            )
                        })
                        .collect();
                    div()
                        .id("profile-admin")
                        .debug_selector(|| "profile-admin".into())
                        .flex()
                        .flex_col()
                        .gap_2()
                        .pt_3()
                        .border_t_1()
                        .border_color(colors.border)
                        .child(
                            div()
                                .text_size(theme.type_scale.small)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.text_muted)
                                .child("Administration"),
                        )
                        .children((!role_choices.is_empty()).then(|| {
                            let roles = role_choices.clone();
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_3()
                                        .child(
                                            div()
                                                .text_size(theme.type_scale.body)
                                                .text_color(colors.text)
                                                .child("Role"),
                                        )
                                        .child(form::toggle_group(
                                            &theme,
                                            "profile-role",
                                            "Role in this workspace",
                                            &options,
                                            selected,
                                            &self.role_focus,
                                            move |index, _, cx| {
                                                let role = roles[index];
                                                role_to
                                                    .update(cx, |dialog, cx| {
                                                        dialog.set_role(role, cx)
                                                    })
                                                    .ok();
                                            },
                                        )),
                                )
                                .child(
                                    div()
                                        .text_size(theme.type_scale.small)
                                        .text_color(colors.text_faint)
                                        .child(workspace_form::role_detail(current)),
                                )
                                .children(match &admin.role_state {
                                    ActionState::InFlight => Some(
                                        div()
                                            .debug_selector(|| "profile-role-saving".into())
                                            .text_size(theme.type_scale.small)
                                            .text_color(colors.text_muted)
                                            .child("Saving…")
                                            .into_any_element(),
                                    ),
                                    ActionState::Failed(message) => Some(
                                        div()
                                            .id("profile-role-failed")
                                            .debug_selector(|| "profile-role-failed".into())
                                            .role(Role::Alert)
                                            .text_size(theme.type_scale.small)
                                            .text_color(colors.danger)
                                            .child(message.clone())
                                            .into_any_element(),
                                    ),
                                    _ => None,
                                })
                        }))
                        .children(can_change_activation.then(|| {
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(div().flex().child(form::button(
                                    &theme,
                                    if admin.deactivated {
                                        "profile-reactivate"
                                    } else {
                                        "profile-deactivate"
                                    },
                                    if admin.deactivated {
                                        "Reactivate"
                                    } else {
                                        "Deactivate"
                                    },
                                    if admin.deactivated {
                                        ButtonKind::Secondary
                                    } else {
                                        ButtonKind::Danger
                                    },
                                    admin.activation_state != ActionState::InFlight,
                                    &self.deactivate_focus,
                                    move |window, cx| {
                                        ask_to
                                            .update(cx, |dialog, cx| {
                                                dialog.ask_to_change_activation(window, cx)
                                            })
                                            .ok();
                                    },
                                )))
                                .children(match &admin.activation_state {
                                    ActionState::Failed(message) => Some(
                                        div()
                                            .id("profile-activation-failed")
                                            .debug_selector(|| "profile-activation-failed".into())
                                            .role(Role::Alert)
                                            .text_size(theme.type_scale.small)
                                            .text_color(colors.danger)
                                            .child(message.clone()),
                                    ),
                                    _ => None,
                                })
                        }))
                }),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .children((is_me).then(|| {
                        form::button(
                            &theme,
                            "profile-edit-status",
                            "Edit status",
                            ButtonKind::Primary,
                            true,
                            &self.primary_focus,
                            move |window, cx| {
                                edit_to
                                    .update(cx, |dialog, cx| dialog.edit_status(window, cx))
                                    .ok();
                            },
                        )
                    }))
                    .children((!is_me && !is_bot && !admin.deactivated).then(|| {
                        form::button(
                            &theme,
                            "profile-message",
                            if opening == ActionState::InFlight {
                                "Opening…"
                            } else {
                                "Message"
                            },
                            ButtonKind::Primary,
                            opening != ActionState::InFlight,
                            &self.primary_focus,
                            move |_, cx| {
                                message_to.update(cx, |dialog, cx| dialog.message(cx)).ok();
                            },
                        )
                    }))
                    .child(form::button(
                        &theme,
                        "profile-close",
                        "Close",
                        ButtonKind::Secondary,
                        true,
                        &self.close_focus,
                        move |window, cx| {
                            close_button_to
                                .update(cx, |dialog, cx| dialog.dismiss(window, cx))
                                .ok();
                        },
                    )),
            )
            .into_any_element();

        let body = if self.confirming {
            let deactivating = !admin.deactivated;
            div()
                .id("profile-confirm")
                .debug_selector(|| "profile-confirm".into())
                .role(Role::Alert)
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .text_size(theme.type_scale.heading)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.text)
                        .child(if deactivating {
                            states::deactivate_question(&name)
                        } else {
                            states::reactivate_question(&name)
                        }),
                )
                .child(
                    div()
                        .text_size(theme.type_scale.body)
                        .text_color(colors.text_muted)
                        .child(if deactivating {
                            states::deactivate_detail(&name, &admin.workspace)
                        } else {
                            states::reactivate_detail(&name)
                        }),
                )
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(form::button(
                            &theme,
                            "profile-confirm-no",
                            "Cancel",
                            ButtonKind::Secondary,
                            true,
                            &self.confirm_no_focus,
                            move |window, cx| {
                                no_to
                                    .update(cx, |dialog, cx| dialog.cancel_confirmation(window, cx))
                                    .ok();
                            },
                        ))
                        .child(form::button(
                            &theme,
                            "profile-confirm-yes",
                            if deactivating {
                                "Deactivate"
                            } else {
                                "Reactivate"
                            },
                            if deactivating {
                                ButtonKind::Danger
                            } else {
                                ButtonKind::Primary
                            },
                            true,
                            &self.confirm_yes_focus,
                            move |window, cx| {
                                yes_to
                                    .update(cx, |dialog, cx| {
                                        dialog.change_activation(deactivating, window, cx)
                                    })
                                    .ok();
                            },
                        )),
                )
                .into_any_element()
        } else {
            body
        };

        dialog_overlay(
            &theme,
            "Profile",
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
