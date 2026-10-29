//! Workspace settings (admins and owners): the workspace's name, who may register, and who may
//! create channels (D22).
//!
//! Nothing applies until Save: the controls hold the person's edits, Save sends one
//! `UpdateWorkspace` with what changed, and the server's answer (or refusal) is shown here. A
//! change made elsewhere (another admin saving) is followed while the person has not edited the
//! field it touches. When the person stops being an admin the dialog closes itself.
//!
//! # Keyboard
//!
//! The name has the keyboard when the dialog opens. Tab order: the name, who may register, who
//! may create channels (Left and Right choose within a control), Save, then Close. Enter in the
//! name saves. Escape closes.

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, ParentElement,
    Render, Role, Styled, Subscription, Window, div, prelude::*,
};
use lazy_client_core::{ActionKind, ActionState, Input};
use lazy_domain::WorkspaceName;
use lazy_domain::workspace::{ChannelCreationPolicy, RegistrationPolicy, WorkspaceSettings};

use super::{any_composing, single_line_field};
use crate::app_model::{AppEvent, AppModel};
use crate::form::{self, ButtonKind, Segment};
use crate::modal::{DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, dialog_overlay};
use crate::states;
use crate::text_input::TextInputEvent;
use crate::theme::Theme;

pub struct WorkspaceSettingsDialog {
    model: Entity<AppModel>,
    trap: ModalTrap,
    name: Entity<crate::text_input::TextInput>,
    registration: RegistrationPolicy,
    channels: ChannelCreationPolicy,
    /// What the workspace had when the controls last agreed with it.
    base_name: String,
    base_settings: WorkspaceSettings,
    /// A save was in flight and has finished without a refusal.
    saved: bool,
    was_saving: bool,
    registration_focus: FocusHandle,
    channels_focus: FocusHandle,
    save_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for WorkspaceSettingsDialog {}

impl WorkspaceSettingsDialog {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = single_line_field("Workspace name", "Workspace name", 256, cx);
        let (base_name, base_settings) = {
            let core = model.read(cx).core();
            (
                core.primary_workspace()
                    .map(|info| info.workspace.name.clone())
                    .unwrap_or_default(),
                core.workspace_settings(),
            )
        };
        name.update(cx, |input, cx| input.set_text(&base_name, cx));
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &name,
            window,
            |this, _input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Changed => {
                    this.saved = false;
                    cx.notify();
                }
                TextInputEvent::Submit => this.save(cx),
                TextInputEvent::Tab { backwards } => this.trap.tab(*backwards, window, cx),
                _ => {}
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &model,
            window,
            |this, _model, _event: &AppEvent, window, cx| this.sync(window, cx),
        ));
        let mut trap = ModalTrap::new(cx);
        let initial = name.focus_handle(cx);
        trap.open(&initial, window, cx);
        Self {
            model,
            trap,
            name,
            registration: base_settings.registration,
            channels: base_settings.channel_creation,
            base_name,
            base_settings,
            saved: false,
            was_saving: false,
            registration_focus: cx.focus_handle().tab_stop(true),
            channels_focus: cx.focus_handle().tab_stop(true),
            save_focus: cx.focus_handle().tab_stop(true),
            _subscriptions: subscriptions,
        }
    }

    /// What the name field holds, for tests.
    pub fn name_text(&self, cx: &gpui::App) -> String {
        self.name.read(cx).text().to_owned()
    }

    /// The chosen policies, for tests.
    pub fn choices(&self) -> (RegistrationPolicy, ChannelCreationPolicy) {
        (self.registration, self.channels)
    }

    /// Which tab stop has the keyboard, for tests.
    pub fn stop(&self, window: &Window, cx: &gpui::App) -> &'static str {
        if self.name.read(cx).is_focused(window) {
            "name"
        } else if self.registration_focus.is_focused(window) {
            "registration"
        } else if self.channels_focus.is_focused(window) {
            "channels"
        } else if self.save_focus.is_focused(window) {
            "save"
        } else {
            "elsewhere"
        }
    }

    fn state(&self, cx: &gpui::App) -> ActionState {
        self.model
            .read(cx)
            .core()
            .action(ActionKind::UpdateWorkspace)
            .clone()
    }

    /// Follows the core: closes when the person is no longer an admin, adopts a change made
    /// elsewhere where the person has not edited, and notices a save finishing.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (can_manage, live_name, live_settings) = {
            let core = self.model.read(cx).core();
            (
                core.can_manage_workspace(),
                core.primary_workspace()
                    .map(|info| info.workspace.name.clone())
                    .unwrap_or_default(),
                core.workspace_settings(),
            )
        };
        if !can_manage {
            self.finish(window, cx);
            return;
        }
        if live_name != self.base_name {
            if self.name.read(cx).text().trim() == self.base_name {
                self.name
                    .update(cx, |input, cx| input.set_text(&live_name, cx));
            }
            self.base_name = live_name;
        }
        if live_settings != self.base_settings {
            if self.registration == self.base_settings.registration {
                self.registration = live_settings.registration;
            }
            if self.channels == self.base_settings.channel_creation {
                self.channels = live_settings.channel_creation;
            }
            self.base_settings = live_settings;
        }
        let saving = self.state(cx) == ActionState::InFlight;
        if self.was_saving && !saving && !matches!(self.state(cx), ActionState::Failed(_)) {
            self.saved = true;
        }
        self.was_saving = saving;
        cx.notify();
    }

    fn name_change(&self, cx: &gpui::App) -> Result<Option<String>, lazy_domain::NameError> {
        let text = self.name.read(cx).text().to_owned();
        let parsed = WorkspaceName::parse(&text)?;
        Ok((parsed.as_str() != self.base_name).then(|| parsed.as_str().to_owned()))
    }

    fn settings_change(&self) -> Option<WorkspaceSettings> {
        let wanted = WorkspaceSettings {
            registration: self.registration,
            channel_creation: self.channels,
        };
        (wanted != self.base_settings).then_some(wanted)
    }

    /// Whether Save would send something.
    pub fn can_save(&self, cx: &gpui::App) -> bool {
        self.state(cx) != ActionState::InFlight
            && matches!(self.name_change(cx), Ok(name) if name.is_some() || self.settings_change().is_some())
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if !self.can_save(cx) {
            return;
        }
        let name = self.name_change(cx).ok().flatten();
        let settings = self.settings_change();
        self.saved = false;
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::UpdateWorkspace { name, settings }, cx)
        });
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if any_composing(&[&self.name], cx) {
            return;
        }
        self.finish(window, cx);
    }

    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::DismissAction(ActionKind::UpdateWorkspace), cx)
        });
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }
}

impl Focusable for WorkspaceSettingsDialog {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl Render for WorkspaceSettingsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let state = self.state(cx);
        let problem = match self.name_change(cx) {
            Err(error) => Some(states::workspace_name_problem(&error)),
            Ok(_) => None,
        };
        let can_save = self.can_save(cx);
        let saving = state == ActionState::InFlight;

        let registration_to = this.clone();
        let channels_to = this.clone();
        let save_to = this.clone();
        let close_to = this.clone();
        let registration = form::segmented(
            &theme,
            "settings-registration",
            &[
                Segment {
                    label: states::REGISTRATION_OPEN.into(),
                    detail: states::REGISTRATION_OPEN_DETAIL.into(),
                },
                Segment {
                    label: states::REGISTRATION_INVITE_ONLY.into(),
                    detail: states::REGISTRATION_INVITE_ONLY_DETAIL.into(),
                },
            ],
            usize::from(self.registration == RegistrationPolicy::InviteOnly),
            &self.registration_focus,
            move |index, _, cx| {
                registration_to
                    .update(cx, |dialog, cx| {
                        dialog.registration = if index == 1 {
                            RegistrationPolicy::InviteOnly
                        } else {
                            RegistrationPolicy::Open
                        };
                        dialog.saved = false;
                        cx.notify();
                    })
                    .ok();
            },
        );
        let channels = form::segmented(
            &theme,
            "settings-channels",
            &[
                Segment {
                    label: states::CHANNELS_EVERYONE.into(),
                    detail: "".into(),
                },
                Segment {
                    label: states::CHANNELS_ADMINS_ONLY.into(),
                    detail: "".into(),
                },
            ],
            usize::from(self.channels == ChannelCreationPolicy::AdminsOnly),
            &self.channels_focus,
            move |index, _, cx| {
                channels_to
                    .update(cx, |dialog, cx| {
                        dialog.channels = if index == 1 {
                            ChannelCreationPolicy::AdminsOnly
                        } else {
                            ChannelCreationPolicy::Everyone
                        };
                        dialog.saved = false;
                        cx.notify();
                    })
                    .ok();
            },
        );
        let heading = |text: &'static str| {
            div()
                .text_size(theme.type_scale.small)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.text_muted)
                .child(text)
        };
        let body = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(form::labeled_field(
                &theme,
                "Name",
                &self.name,
                None,
                problem.map(Into::into),
                window,
                cx,
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(heading("Who can create accounts"))
                    .child(registration),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(heading("Who can create channels"))
                    .child(channels),
            )
            .children(match &state {
                ActionState::Failed(message) => Some(
                    div()
                        .id("settings-failed")
                        .debug_selector(|| "settings-failed".into())
                        .role(Role::Alert)
                        .text_size(theme.type_scale.small)
                        .text_color(colors.danger)
                        .child(message.clone()),
                ),
                _ => None,
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_3()
                    .children((self.saved && !saving).then(|| {
                        div()
                            .id("settings-saved")
                            .debug_selector(|| "settings-saved".into())
                            .role(Role::Status)
                            .text_size(theme.type_scale.small)
                            .text_color(colors.text_muted)
                            .child(states::SETTINGS_SAVED)
                    }))
                    .child(form::button(
                        &theme,
                        "settings-save",
                        if saving { "Saving…" } else { "Save" },
                        ButtonKind::Primary,
                        can_save,
                        &self.save_focus,
                        move |_, cx| {
                            save_to.update(cx, |dialog, cx| dialog.save(cx)).ok();
                        },
                    )),
            )
            .into_any_element();

        dialog_overlay(
            &theme,
            "Workspace settings",
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
    }
}
