//! The people dialog: pick people to start a direct or group conversation, to start a *new*
//! group from an existing one, or to add to a channel.

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, SharedString,
    Styled, Subscription, Window, div, prelude::*,
};
use lazy_client_core::{ActionKind, ActionState, Change, Input};
use lazy_domain::{ConversationId, UserId, conversation::MAX_PARTICIPANTS};

use crate::app_model::{AppEvent, AppModel};
use crate::form::{self, ButtonKind};
use crate::modal::{DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, dialog_overlay};
use crate::people_picker::{PeoplePicker, Person, PickerEvent};
use crate::theme::Theme;

/// What the dialog is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeopleMode {
    /// Start (or find) the direct or group conversation with the chosen people. `prefill` and
    /// `from` are set when this is "Add people" on an existing direct or group conversation:
    /// the result is a *new* conversation with the larger set and no history.
    NewConversation {
        prefill: Vec<UserId>,
        from: Option<ConversationId>,
    },
    /// Add the chosen people to a channel.
    AddToChannel(ConversationId),
}

pub struct PeopleDialog {
    model: Entity<AppModel>,
    mode: PeopleMode,
    trap: ModalTrap,
    picker: Entity<PeoplePicker>,
    cancel_focus: FocusHandle,
    submit_focus: FocusHandle,
    was_in_flight: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for PeopleDialog {}

impl PeopleDialog {
    pub fn new(
        model: Entity<AppModel>,
        mode: PeopleMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let people: Vec<Person> = {
            let core = model.read(cx).core();
            let me = core.me().map(|user| user.id);
            let mut people: Vec<Person> = core
                .members()
                .into_iter()
                .filter(|user| Some(user.id) != me)
                .map(|user| Person {
                    id: user.id,
                    name: user.display_name.to_string().into(),
                    detail: format!("@{}", user.username).into(),
                    status: crate::status_mark::StatusMark::lookup(core, user.id),
                })
                .collect();
            people.sort_by_key(|person| person.name.to_lowercase());
            people
        };
        let picker =
            cx.new(|cx| PeoplePicker::new(people, MAX_PARTICIPANTS - 1, "Search people", cx));
        if let PeopleMode::NewConversation { prefill, .. } = &mode {
            let prefill = prefill.clone();
            picker.update(cx, |picker, cx| picker.set_selected(prefill, cx));
        }
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &picker,
            window,
            |this, _picker, event: &PickerEvent, window, cx| match event {
                PickerEvent::Changed => cx.notify(),
                PickerEvent::Submit => this.submit(cx),
                PickerEvent::Tab { backwards } => this.trap.tab(*backwards, window, cx),
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &model,
            window,
            |this, _model, event: &AppEvent, window, cx| {
                let AppEvent::Changed(changes) = event;
                if changes
                    .iter()
                    .any(|c| matches!(c, Change::ConversationOpened(_)))
                {
                    this.finish(window, cx);
                    return;
                }
                // "Add to channel" has no navigation to wait for: success is the action settling.
                let state = this.state(cx);
                if matches!(this.mode, PeopleMode::AddToChannel(_)) {
                    if state == ActionState::InFlight {
                        this.was_in_flight = true;
                    } else if this.was_in_flight && state == ActionState::Idle {
                        this.finish(window, cx);
                        return;
                    }
                }
                cx.notify();
            },
        ));
        let mut trap = ModalTrap::new(cx);
        let initial = picker.read(cx).focus_handle(cx);
        trap.open(&initial, window, cx);
        Self {
            model,
            mode,
            trap,
            picker,
            cancel_focus: cx.focus_handle().tab_stop(true),
            submit_focus: cx.focus_handle().tab_stop(true),
            was_in_flight: false,
            _subscriptions: subscriptions,
        }
    }

    fn kind(&self) -> ActionKind {
        match &self.mode {
            PeopleMode::NewConversation { .. } => ActionKind::OpenConversation,
            PeopleMode::AddToChannel(id) => ActionKind::AddMembers(*id),
        }
    }

    fn state(&self, cx: &gpui::App) -> ActionState {
        self.model.read(cx).core().action(self.kind()).clone()
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let selected = self.picker.read(cx).selected().to_vec();
        if selected.is_empty() || self.state(cx) == ActionState::InFlight {
            return;
        }
        let input = match &self.mode {
            PeopleMode::NewConversation { .. } => Input::OpenConversation {
                participants: selected,
            },
            PeopleMode::AddToChannel(id) => Input::AddMembers {
                id: *id,
                users: selected,
            },
        };
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let search = self.picker.read(cx).input().clone();
        if search.read(cx).is_composing() {
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

    fn title(&self, cx: &gpui::App) -> SharedString {
        match &self.mode {
            PeopleMode::NewConversation { from: Some(_), .. } => "Add people".into(),
            PeopleMode::NewConversation { .. } => "New message".into(),
            PeopleMode::AddToChannel(id) => {
                let name = self.model.read(cx).core().conversation_title(*id);
                format!("Add people to #{name}").into()
            }
        }
    }
}

impl Render for PeopleDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        let state = self.state(cx);
        let in_flight = state == ActionState::InFlight;
        let selected = self.picker.read(cx).selected().to_vec();
        let names: Vec<String> = {
            let core = self.model.read(cx).core();
            selected
                .iter()
                .map(|id| core.display_name(*id).to_owned())
                .collect()
        };

        let (summary, note): (String, Option<String>) = match &self.mode {
            PeopleMode::NewConversation { from, .. } => {
                let summary = match names.len() {
                    0 => "Pick one person for a direct message, or several for a group.".to_owned(),
                    1 => format!("Direct message with {}.", names[0]),
                    _ => format!("Group conversation with {}.", names.join(", ")),
                };
                let note = from.map(|old| {
                    let title = self.model.read(cx).core().conversation_title(old);
                    format!(
                        "This starts a new conversation. It won't include the earlier history from \"{title}\", which stays exactly as it is."
                    )
                });
                (summary, note)
            }
            PeopleMode::AddToChannel(_) => (
                if names.is_empty() {
                    "Pick the people to add.".to_owned()
                } else {
                    format!(
                        "Adding {}. People already in the channel are skipped.",
                        names.join(", ")
                    )
                },
                None,
            ),
        };
        let submit_label = match (&self.mode, in_flight) {
            (_, true) => "Working…",
            (PeopleMode::NewConversation { from: Some(_), .. }, _) => "Start new conversation",
            (PeopleMode::NewConversation { .. }, _) => "Start conversation",
            (PeopleMode::AddToChannel(_), _) => "Add",
        };

        let this = cx.entity().downgrade();
        let cancel_to = this.clone();
        let submit_to = this.clone();
        let close_to = this.clone();
        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.picker.clone())
            .child(
                div()
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_muted)
                    .child(summary),
            )
            .children(note.map(|note| {
                div()
                    .px_3()
                    .py_2()
                    .rounded(theme.radii.small)
                    .bg(colors.warning_surface)
                    .text_size(theme.type_scale.small)
                    .text_color(colors.warning)
                    .child(note)
            }))
            .children(match &state {
                ActionState::Failed(message) => Some(
                    div()
                        .text_size(theme.type_scale.small)
                        .text_color(colors.danger)
                        .child(message.clone()),
                ),
                _ => None,
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        &theme,
                        "people-cancel",
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
                        "people-submit",
                        submit_label,
                        ButtonKind::Primary,
                        !selected.is_empty() && !in_flight,
                        &self.submit_focus,
                        move |_, cx| {
                            submit_to.update(cx, |dialog, cx| dialog.submit(cx)).ok();
                        },
                    )),
            )
            .into_any_element();

        dialog_overlay(
            &theme,
            self.title(cx),
            480.,
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
