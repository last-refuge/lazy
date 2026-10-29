//! "Add a workspace": create one (a name) or join an existing one with an invitation code (D23).
//!
//! Both end the same way: the workspace is added to the rail and becomes the active one, and the
//! dialog closes. Until then it shows progress, and a refusal in the server's own words in place.
//! The invitation code is typed into a field of its own and handed to the core; it is never put
//! in a label, a tooltip, or a log line.
//!
//! # Keyboard
//!
//! The two choices are one tab stop (Left and Right choose, and the keyboard stays on the choice);
//! the name field has the keyboard when the dialog opens and Enter in a field does what the button
//! does. Tab order: the choice, the field, the button. Escape closes, and the keyboard goes back
//! to where it was.

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    Role, Styled, Subscription, Window, div, prelude::*,
};
use lazy_client_core::{ActionKind, ActionState, Input};
use lazy_domain::WorkspaceId;

use super::{any_composing, single_line_field};
use crate::app_model::{AppEvent, AppModel};
use crate::form::{self, ButtonKind};
use crate::modal::{DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, dialog_overlay};
use crate::states;
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::Theme;

/// What the person is adding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Way {
    Create,
    Join,
}

pub struct AddWorkspaceDialog {
    model: Entity<AppModel>,
    trap: ModalTrap,
    way: Way,
    name: Entity<TextInput>,
    code: Entity<TextInput>,
    /// The workspace that was active when the dialog opened: another one being active means the
    /// new one arrived.
    opened_on: Option<WorkspaceId>,
    way_focus: FocusHandle,
    submit_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for AddWorkspaceDialog {}

impl AddWorkspaceDialog {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = single_line_field("Workspace name", "Workspace name", 256, cx);
        let code = single_line_field(states::INVITE_CODE_PLACEHOLDER, "Invitation code", 256, cx);
        let mut subscriptions = Vec::new();
        for input in [&name, &code] {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                |this, _input, event: &TextInputEvent, window, cx| match event {
                    TextInputEvent::Submit => this.submit(cx),
                    TextInputEvent::Tab { backwards } => this.trap.tab(*backwards, window, cx),
                    TextInputEvent::Changed => cx.notify(),
                    _ => {}
                },
            ));
        }
        subscriptions.push(cx.subscribe_in(
            &model,
            window,
            |this, _model, _event: &AppEvent, window, cx| this.sync(window, cx),
        ));
        let opened_on = model.read(cx).core().active_workspace();
        let mut trap = ModalTrap::new(cx);
        let initial = name.focus_handle(cx);
        trap.open(&initial, window, cx);
        Self {
            model,
            trap,
            way: Way::Create,
            name,
            code,
            opened_on,
            way_focus: cx.focus_handle().tab_stop(true),
            submit_focus: cx.focus_handle().tab_stop(true),
            _subscriptions: subscriptions,
        }
    }

    // ----- For tests -----

    pub fn way(&self) -> Way {
        self.way
    }

    /// Which tab stop has the keyboard.
    pub fn stop(&self, window: &Window, cx: &gpui::App) -> &'static str {
        if self.name.read(cx).is_focused(window) {
            "name"
        } else if self.code.read(cx).is_focused(window) {
            "code"
        } else if self.way_focus.is_focused(window) {
            "way"
        } else if self.submit_focus.is_focused(window) {
            "submit"
        } else {
            "elsewhere"
        }
    }

    fn kind(&self) -> ActionKind {
        match self.way {
            Way::Create => ActionKind::CreateWorkspace,
            Way::Join => ActionKind::AcceptInvitation,
        }
    }

    fn state(&self, cx: &gpui::App) -> ActionState {
        self.model.read(cx).core().action(self.kind()).clone()
    }

    fn field(&self) -> &Entity<TextInput> {
        match self.way {
            Way::Create => &self.name,
            Way::Join => &self.code,
        }
    }

    /// The new workspace is active: the job is done.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = self.model.read(cx).core().active_workspace();
        if now.is_some() && now != self.opened_on {
            self.finish(window, cx);
            return;
        }
        cx.notify();
    }

    fn can_submit(&self, cx: &gpui::App) -> bool {
        !self.field().read(cx).text().trim().is_empty() && self.state(cx) != ActionState::InFlight
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if !self.can_submit(cx) {
            return;
        }
        let text = self.field().read(cx).text().trim().to_owned();
        let input = match self.way {
            Way::Create => Input::CreateWorkspace { name: text },
            Way::Join => Input::AcceptInvitation { code: text },
        };
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
    }

    fn choose(&mut self, way: Way, cx: &mut Context<Self>) {
        if self.way == way {
            return;
        }
        // The keyboard stays on the choice, so Left and Right flip back and forth; Tab goes on to
        // the field.
        self.way = way;
        cx.notify();
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if any_composing(&[&self.name, &self.code], cx) {
            return;
        }
        self.finish(window, cx);
    }

    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::DismissAction(ActionKind::CreateWorkspace), cx);
            model.dispatch(Input::DismissAction(ActionKind::AcceptInvitation), cx);
        });
        // Nothing typed here outlives the dialog.
        self.name.update(cx, |input, cx| input.clear(cx));
        self.code.update(cx, |input, cx| input.clear(cx));
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }
}

impl Focusable for AddWorkspaceDialog {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl Render for AddWorkspaceDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let way_to = this.clone();
        let submit_to = this.clone();
        let close_to = this.clone();
        let state = self.state(cx);
        let busy = state == ActionState::InFlight;
        let creating = self.way == Way::Create;
        let body = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(form::toggle_group(
                &theme,
                "add-workspace-way",
                "What to do",
                &[
                    ("Create a workspace", "add-workspace-create"),
                    ("Join with an invitation code", "add-workspace-join"),
                ],
                usize::from(!creating),
                &self.way_focus,
                move |index, _window, cx| {
                    way_to
                        .update(cx, |dialog, cx| {
                            dialog.choose(if index == 0 { Way::Create } else { Way::Join }, cx)
                        })
                        .ok();
                },
            ))
            .child(
                div()
                    .text_size(theme.type_scale.body)
                    .text_color(colors.text_muted)
                    .child(if creating {
                        "A new workspace has you as its owner, with a #general channel to start in."
                    } else {
                        "Paste the invitation code you were given."
                    }),
            )
            .child(if creating {
                form::labeled_field(&theme, "Name", &self.name, None, None, window, cx)
            } else {
                form::labeled_field(
                    &theme,
                    "Invitation code",
                    &self.code,
                    None,
                    None,
                    window,
                    cx,
                )
            })
            .children(match &state {
                ActionState::Failed(message) => Some(
                    div()
                        .id("add-workspace-failed")
                        .debug_selector(|| "add-workspace-failed".into())
                        .role(Role::Alert)
                        .text_size(theme.type_scale.small)
                        .text_color(colors.danger)
                        .child(message.clone()),
                ),
                _ => None,
            })
            .child(div().flex().justify_end().child(form::button(
                &theme,
                "add-workspace-submit",
                match (creating, busy) {
                    (true, false) => "Create workspace",
                    (true, true) => "Creating…",
                    (false, false) => "Join workspace",
                    (false, true) => "Joining…",
                },
                ButtonKind::Primary,
                self.can_submit(cx),
                &self.submit_focus,
                move |_, cx| {
                    submit_to.update(cx, |dialog, cx| dialog.submit(cx)).ok();
                },
            )))
            .into_any_element();
        dialog_overlay(
            &theme,
            "Add a workspace",
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
