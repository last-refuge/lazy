//! User groups: who can mention a team at once. For admins and owners.
//!
//! One dialog with three views. The list shows the workspace's groups (name, `@handle`, member
//! count) and acts on the selected row; the editor creates or changes one (handle, name, and the
//! people in it); a confirmation precedes deleting. Everything is keyboard-operable: the list is
//! one tab stop where Up and Down select, Enter edits, and Delete removes; Escape steps back from
//! the editor or confirmation to the list, and closes the dialog from the list.

use gpui::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    KeyDownEvent, ParentElement, Render, Role, SharedString, Styled, Subscription, Window, div,
    prelude::*, px,
};
use lazy_client_core::{ActionKind, ActionState, Input};
use lazy_domain::{GroupId, MAX_GROUP_MEMBERS};
use lazy_protocol::UserGroup;

use super::{any_composing, single_line_field};
use crate::app_model::{AppEvent, AppModel};
use crate::form::{self, ButtonKind};
use crate::group_form::{self, HandleCheck, NameCheck};
use crate::modal::{DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, dialog_overlay};
use crate::people_picker::{PeoplePicker, Person, PickerEvent};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::Theme;

/// Which view the dialog shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    List,
    /// Creating (`None`) or changing a group.
    Editor(Option<GroupId>),
    ConfirmDelete(GroupId),
}

pub struct GroupsDialog {
    model: Entity<AppModel>,
    trap: ModalTrap,
    view: View,
    /// The row the list acts on.
    selected: usize,
    handle: Entity<TextInput>,
    name: Entity<TextInput>,
    people: Entity<PeoplePicker>,
    /// The action whose success returns to the list, and whether it has been seen starting.
    waiting: Option<(ActionKind, bool)>,
    list_focus: FocusHandle,
    new_focus: FocusHandle,
    edit_focus: FocusHandle,
    delete_focus: FocusHandle,
    close_focus: FocusHandle,
    cancel_focus: FocusHandle,
    save_focus: FocusHandle,
    confirm_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for GroupsDialog {}

impl GroupsDialog {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let handle = single_line_field("e.g. design-team", "Group handle", 64, cx);
        let name = single_line_field("e.g. Design team", "Group name", 128, cx);
        let people_list: Vec<Person> = {
            let core = model.read(cx).core();
            let mut people: Vec<Person> = core
                .members()
                .into_iter()
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
        let people =
            cx.new(|cx| PeoplePicker::new(people_list, MAX_GROUP_MEMBERS, "Add people", cx));

        let mut subscriptions = Vec::new();
        for input in [&handle, &name] {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                |this, _input, event: &TextInputEvent, window, cx| match event {
                    TextInputEvent::Submit => this.save(window, cx),
                    TextInputEvent::Tab { backwards } => this.trap.tab(*backwards, window, cx),
                    TextInputEvent::Changed => cx.notify(),
                    TextInputEvent::UpWhenEmpty
                    | TextInputEvent::PasteAttachments
                    | TextInputEvent::Suggestion(_) => {}
                },
            ));
        }
        subscriptions.push(cx.subscribe_in(
            &people,
            window,
            |this, _picker, event: &PickerEvent, window, cx| match event {
                PickerEvent::Changed => cx.notify(),
                PickerEvent::Submit => this.save(window, cx),
                PickerEvent::Tab { backwards } => this.trap.tab(*backwards, window, cx),
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
        let list_focus = cx.focus_handle().tab_stop(true);
        let new_focus = cx.focus_handle().tab_stop(true);
        // An empty list has nothing to choose, so the dialog starts on the way to make one.
        let nothing_yet = model.read(cx).core().groups().is_empty();
        trap.open(
            if nothing_yet { &new_focus } else { &list_focus },
            window,
            cx,
        );
        Self {
            model,
            trap,
            view: View::List,
            selected: 0,
            handle,
            name,
            people,
            waiting: None,
            list_focus,
            new_focus,
            edit_focus: cx.focus_handle().tab_stop(true),
            delete_focus: cx.focus_handle().tab_stop(true),
            close_focus: cx.focus_handle().tab_stop(true),
            cancel_focus: cx.focus_handle().tab_stop(true),
            save_focus: cx.focus_handle().tab_stop(true),
            confirm_focus: cx.focus_handle().tab_stop(true),
            _subscriptions: subscriptions,
        }
    }

    fn groups(&self, cx: &gpui::App) -> Vec<UserGroup> {
        self.model.read(cx).core().groups().to_vec()
    }

    fn selected_group(&self, cx: &gpui::App) -> Option<UserGroup> {
        self.groups(cx).get(self.selected).cloned()
    }

    fn action(&self, kind: ActionKind, cx: &gpui::App) -> ActionState {
        self.model.read(cx).core().action(kind).clone()
    }

    // ----- Moving between views -----

    fn show_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.view = View::List;
        self.waiting = None;
        let count = self.groups(cx).len();
        self.selected = self.selected.min(count.saturating_sub(1));
        let target = if count == 0 {
            &self.new_focus
        } else {
            &self.list_focus
        };
        window.focus(target, cx);
        cx.notify();
    }

    fn open_editor(&mut self, group: Option<GroupId>, window: &mut Window, cx: &mut Context<Self>) {
        // An answer left over from an earlier visit is not about this one.
        self.dismiss_actions(group, cx);
        let existing = group.and_then(|id| self.groups(cx).into_iter().find(|g| g.id == id));
        self.handle.update(cx, |input, cx| {
            input.set_text(existing.as_ref().map_or("", |g| g.handle.as_str()), cx)
        });
        self.name.update(cx, |input, cx| {
            input.set_text(existing.as_ref().map_or("", |g| g.name.as_str()), cx)
        });
        let members = existing.map(|g| g.members).unwrap_or_default();
        self.people
            .update(cx, |picker, cx| picker.set_selected(members, cx));
        self.view = View::Editor(group);
        self.waiting = None;
        let handle = self.handle.focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(group) = self.selected_group(cx) {
            self.view = View::ConfirmDelete(group.id);
            window.focus(&self.cancel_focus, cx);
            cx.notify();
        }
    }

    // ----- Actions -----

    /// The editor's content, if it can be saved.
    fn draft(&self, cx: &gpui::App) -> Result<group_form::GroupDraft, Option<String>> {
        group_form::draft(
            self.handle.read(cx).text(),
            self.name.read(cx).text(),
            self.people.read(cx).selected(),
        )
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let View::Editor(group) = self.view else {
            return;
        };
        let kind = match group {
            Some(id) => ActionKind::UpdateGroup(id),
            None => ActionKind::CreateGroup,
        };
        if self.waiting.is_some() || self.action(kind, cx) == ActionState::InFlight {
            return;
        }
        let Ok(draft) = self.draft(cx) else {
            return;
        };
        let handle = draft.handle.to_string();
        let name = draft.name.to_string();
        let members = draft.members;
        let input = match group {
            Some(id) => {
                let Some(existing) = self.groups(cx).into_iter().find(|g| g.id == id) else {
                    // Deleted by someone else while open.
                    self.show_list(window, cx);
                    return;
                };
                let same_members = existing.members.len() == members.len()
                    && existing.members.iter().all(|m| members.contains(m));
                if existing.handle.as_str() == handle
                    && existing.name.as_str() == name
                    && same_members
                {
                    // Nothing changed: there is nothing to send.
                    self.show_list(window, cx);
                    return;
                }
                Input::UpdateGroup {
                    id,
                    handle: (existing.handle.as_str() != handle).then_some(handle),
                    name: (existing.name.as_str() != name).then_some(name),
                    members: (!same_members).then_some(members),
                }
            }
            None => Input::CreateGroup {
                handle,
                name,
                members,
            },
        };
        self.waiting = Some((kind, false));
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        let View::ConfirmDelete(id) = self.view else {
            return;
        };
        let kind = ActionKind::DeleteGroup(id);
        if self.waiting.is_some() {
            return;
        }
        self.waiting = Some((kind, false));
        self.model
            .update(cx, |model, cx| model.dispatch(Input::DeleteGroup(id), cx));
    }

    /// Goes back to the list when the action the dialog was waiting for has succeeded, and forgets
    /// it when it failed (its message stays on screen).
    fn settle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((kind, started)) = self.waiting else {
            return;
        };
        match self.action(kind, cx) {
            ActionState::InFlight => self.waiting = Some((kind, true)),
            ActionState::Idle if started => {
                self.model.update(cx, |model, cx| {
                    model.dispatch(Input::DismissAction(kind), cx)
                });
                self.show_list(window, cx);
            }
            ActionState::Failed(_) => self.waiting = None,
            ActionState::Idle => {}
        }
    }

    // ----- Leaving -----

    fn composing(&self, cx: &gpui::App) -> bool {
        any_composing(
            &[&self.handle, &self.name, self.people.read(cx).input()],
            cx,
        )
    }

    /// Escape: back to the list from the editor or confirmation, closed from the list.
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.composing(cx) {
            return;
        }
        match self.view {
            View::List => self.finish(window, cx),
            View::Editor(group) => {
                self.dismiss_actions(group, cx);
                self.show_list(window, cx);
            }
            View::ConfirmDelete(id) => {
                self.model.update(cx, |model, cx| {
                    model.dispatch(Input::DismissAction(ActionKind::DeleteGroup(id)), cx)
                });
                self.show_list(window, cx);
            }
        }
    }

    fn dismiss_actions(&mut self, group: Option<GroupId>, cx: &mut Context<Self>) {
        let kind = match group {
            Some(id) => ActionKind::UpdateGroup(id),
            None => ActionKind::CreateGroup,
        };
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::DismissAction(kind), cx)
        });
    }

    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }

    // ----- Keys -----

    fn on_list_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.is_held || event.keystroke.modifiers.modified() {
            return;
        }
        let count = self.groups(cx).len();
        let handled = match event.keystroke.key.as_str() {
            "up" => {
                self.selected = self.selected.saturating_sub(1);
                true
            }
            "down" => {
                self.selected = (self.selected + 1).min(count.saturating_sub(1));
                true
            }
            "home" => {
                self.selected = 0;
                true
            }
            "end" => {
                self.selected = count.saturating_sub(1);
                true
            }
            "enter" => {
                if let Some(group) = self.selected_group(cx) {
                    self.open_editor(Some(group.id), window, cx);
                }
                true
            }
            "delete" | "backspace" => {
                self.confirm_delete(window, cx);
                true
            }
            _ => false,
        };
        if handled {
            cx.stop_propagation();
            cx.notify();
        }
    }

    // ----- Drawing -----

    fn list_view(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let groups = self.groups(cx);
        let this = cx.entity().downgrade();
        let selected_exists = self.selected < groups.len();

        let mut list = div()
            .id("groups-list")
            .debug_selector(|| "groups-list".into())
            // With nothing in it the list is not a place to stop.
            .when(!groups.is_empty(), |list| {
                list.track_focus(&self.list_focus)
            })
            .role(Role::List)
            .aria_label("User groups")
            .flex()
            .flex_col()
            .max_h(px(280.))
            .overflow_y_scroll()
            .rounded(theme.radii.medium)
            .border_1()
            .border_color(colors.border_strong)
            .focus_visible(|style| style.border_color(colors.focus_ring))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_list_key(event, window, cx)
            }));
        if groups.is_empty() {
            list = list.child(
                div()
                    .px_4()
                    .py_5()
                    .text_size(theme.type_scale.body)
                    .text_color(colors.text_muted)
                    .child("No groups yet. Create one to mention a whole team at once."),
            );
        }
        for (index, group) in groups.iter().enumerate() {
            let chosen = index == self.selected;
            let select_to = this.clone();
            let count = group.members.len();
            list = list.child(
                div()
                    .id(gpui::ElementId::NamedInteger(
                        "group-row".into(),
                        index as u64,
                    ))
                    .debug_selector(move || format!("group-row-{index}"))
                    .role(Role::ListItem)
                    .aria_label(SharedString::from(format!(
                        "{}, @{}, {} {}",
                        group.name,
                        group.handle,
                        count,
                        if count == 1 { "member" } else { "members" }
                    )))
                    .flex()
                    .items_baseline()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .cursor_pointer()
                    .when(chosen, |row| row.bg(colors.selection))
                    .hover(|style| style.bg(colors.surface_hover))
                    .on_click(move |_, window, cx| {
                        select_to
                            .update(cx, |dialog, cx| {
                                dialog.selected = index;
                                window.focus(&dialog.list_focus, cx);
                                cx.notify();
                            })
                            .ok();
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(theme.type_scale.body)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.text)
                            .child(group.name.to_string()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(theme.type_scale.small)
                            .text_color(colors.text_muted)
                            .child(format!("@{}", group.handle)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(84.))
                            .text_right()
                            .text_size(theme.type_scale.small)
                            .text_color(colors.text_faint)
                            .child(format!(
                                "{count} {}",
                                if count == 1 { "member" } else { "members" }
                            )),
                    ),
            );
        }

        let close_to = this.clone();
        let new_to = this.clone();
        let edit_to = this.clone();
        let delete_to = this;
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(list)
            .when(!groups.is_empty(), |column| {
                column.child(
                    div()
                        .text_size(theme.type_scale.small)
                        .text_color(colors.text_faint)
                        .child("Up and Down choose a group, Enter edits it, Delete removes it."),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_between()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "groups-close",
                        "Close",
                        ButtonKind::Secondary,
                        true,
                        &self.close_focus,
                        move |window, cx| {
                            close_to
                                .update(cx, |dialog, cx| dialog.finish(window, cx))
                                .ok();
                        },
                    ))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(form::button(
                                theme,
                                "groups-delete",
                                "Delete…",
                                ButtonKind::Danger,
                                selected_exists,
                                &self.delete_focus,
                                move |window, cx| {
                                    delete_to
                                        .update(cx, |dialog, cx| dialog.confirm_delete(window, cx))
                                        .ok();
                                },
                            ))
                            .child(form::button(
                                theme,
                                "groups-edit",
                                "Edit",
                                ButtonKind::Secondary,
                                selected_exists,
                                &self.edit_focus,
                                move |window, cx| {
                                    edit_to
                                        .update(cx, |dialog, cx| {
                                            if let Some(group) = dialog.selected_group(cx) {
                                                dialog.open_editor(Some(group.id), window, cx);
                                            }
                                        })
                                        .ok();
                                },
                            ))
                            .child(form::button(
                                theme,
                                "groups-new",
                                "New group",
                                ButtonKind::Primary,
                                true,
                                &self.new_focus,
                                move |window, cx| {
                                    new_to
                                        .update(cx, |dialog, cx| {
                                            dialog.open_editor(None, window, cx)
                                        })
                                        .ok();
                                },
                            )),
                    ),
            )
            .into_any_element()
    }

    fn editor_view(
        &self,
        group: Option<GroupId>,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let kind = match group {
            Some(id) => ActionKind::UpdateGroup(id),
            None => ActionKind::CreateGroup,
        };
        let state = self.action(kind, cx);
        let saving =
            state == ActionState::InFlight || self.waiting.is_some_and(|(_, started)| started);
        let typed_handle = self.handle.read(cx).text().to_owned();
        let typed_name = self.name.read(cx).text().to_owned();

        let (handle_hint, handle_error): (Option<SharedString>, Option<SharedString>) =
            match group_form::check_handle(&typed_handle) {
                HandleCheck::Empty => (
                    Some("People type @ and this to mention the group. Letters, digits, dots, underscores, and dashes.".into()),
                    None,
                ),
                HandleCheck::Valid { handle, changed } => (
                    Some(
                        if changed {
                            format!("Will be saved as @{handle}")
                        } else {
                            format!("People will mention it as @{handle}")
                        }
                        .into(),
                    ),
                    None,
                ),
                HandleCheck::Problem(why) => (None, Some(why.into())),
            };
        let name_error: Option<SharedString> = match group_form::check_name(&typed_name) {
            NameCheck::Problem(why) => Some(why.into()),
            NameCheck::Empty | NameCheck::Valid(_) => None,
        };
        let draft = self.draft(cx);
        let members_error: Option<String> = match &draft {
            Err(Some(why)) if why.contains("people") => Some(why.clone()),
            _ => None,
        };
        // The core's or the server's answer: a handle already taken, a limit, no permission.
        let core_error = match &state {
            ActionState::Failed(message) => Some(message.clone()),
            _ => None,
        };
        let can_save = draft.is_ok() && !saving;
        let count = self.people.read(cx).selected().len();

        let this = cx.entity().downgrade();
        let cancel_to = this.clone();
        let save_to = this.clone();

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(form::labeled_field(
                theme,
                "Handle",
                &self.handle,
                handle_hint,
                handle_error,
                window,
                cx,
            ))
            .child(form::labeled_field(
                theme, "Name", &self.name, None, name_error, window, cx,
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(theme.type_scale.small)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.text_muted)
                            .child(format!("People · {count} of {MAX_GROUP_MEMBERS} at most")),
                    )
                    .child(self.people.clone())
                    .children(members_error.map(|why| {
                        div()
                            .text_size(theme.type_scale.small)
                            .text_color(colors.danger)
                            .child(why)
                    })),
            )
            .children(core_error.map(|message| {
                div()
                    .debug_selector(|| "group-error".into())
                    .text_size(theme.type_scale.small)
                    .text_color(colors.danger)
                    .child(message)
            }))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "group-cancel",
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
                        theme,
                        "group-save",
                        if saving { "Saving…" } else { "Save" },
                        ButtonKind::Primary,
                        can_save,
                        &self.save_focus,
                        move |window, cx| {
                            save_to
                                .update(cx, |dialog, cx| dialog.save(window, cx))
                                .ok();
                        },
                    )),
            )
            .into_any_element()
    }

    fn confirm_view(&self, id: GroupId, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let group = self.groups(cx).into_iter().find(|g| g.id == id);
        let state = self.action(ActionKind::DeleteGroup(id), cx);
        let working = state == ActionState::InFlight;
        let error = match &state {
            ActionState::Failed(message) => Some(message.clone()),
            _ => None,
        };
        let this = cx.entity().downgrade();
        let cancel_to = this.clone();
        let delete_to = this;
        let label = group.map_or_else(
            || "this group".to_owned(),
            |g| format!("{} (@{})", g.name, g.handle),
        );
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_size(theme.type_scale.body)
                    .text_color(colors.text)
                    .child(format!(
                        "Delete {label}? Nobody will be able to mention it any more. Messages that \
                         already mentioned it keep the people they notified."
                    )),
            )
            .children(error.map(|message| {
                div()
                    .debug_selector(|| "group-error".into())
                    .text_size(theme.type_scale.small)
                    .text_color(colors.danger)
                    .child(message)
            }))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "group-delete-cancel",
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
                        theme,
                        "group-delete-confirm",
                        if working {
                            "Deleting…"
                        } else {
                            "Delete group"
                        },
                        ButtonKind::Danger,
                        !working,
                        &self.confirm_focus,
                        move |_, cx| {
                            delete_to.update(cx, |dialog, cx| dialog.delete(cx)).ok();
                        },
                    )),
            )
            .into_any_element()
    }
}

impl Render for GroupsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let this = cx.entity().downgrade();
        let close_to = this.clone();
        let (title, body, width) = match self.view {
            View::List => ("User groups", self.list_view(&theme, cx), 560.),
            View::Editor(None) => (
                "New group",
                self.editor_view(None, &theme, window, cx),
                520.,
            ),
            View::Editor(Some(id)) => (
                "Edit group",
                self.editor_view(Some(id), &theme, window, cx),
                520.,
            ),
            View::ConfirmDelete(id) => ("Delete group", self.confirm_view(id, &theme, cx), 460.),
        };
        dialog_overlay(
            &theme,
            title,
            width,
            &self.trap,
            body,
            "Close",
            move |window, cx| {
                close_to
                    .update(cx, |dialog, cx| {
                        // The corner button always closes the dialog, from any view.
                        dialog.finish(window, cx)
                    })
                    .ok();
            },
        )
        .on_action(cx.listener(|this, _: &DialogDismiss, window, cx| this.dismiss(window, cx)))
        .on_action(cx.listener(|this, _: &DialogNext, window, cx| this.trap.tab(false, window, cx)))
        .on_action(cx.listener(|this, _: &DialogPrev, window, cx| this.trap.tab(true, window, cx)))
    }
}
