//! The "Server" row: which server the app talks to, whether it can be reached, and a way to change
//! it (D25). It sits under the sign-in and register forms, on the unreachable-server screen, and on
//! the no-access screen, because an installed app has no other way to point itself at a server.
//!
//! The address is shown muted with a "Change…" button. Change turns the address into a text field
//! (Enter connects, Escape goes back), and what the check finds is said in one line under it:
//! "Checking…", "Connected", "Can't reach this server", or that the server needs a newer or older
//! Lazy. When the address comes from the environment it is shown locked with the reason, since a
//! value typed here would be ignored at the next start.
//!
//! # Keyboard
//!
//! The row's stops come after the form's own, so the sign-in order is unchanged: "Change…" is one
//! stop. While changing, the field has the keyboard, then Connect and Cancel. Enter in the field
//! connects; Escape cancels (and, while an input method has text in composition, cancels only the
//! composition). Cancel, and a change that was made, give the keyboard back to "Change…".

use gpui::{
    Context, Entity, FocusHandle, Focusable as _, FontWeight, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, Role, Styled, Subscription, Window, div, prelude::*, px,
};
use lazy_client_core::{ClientCore, Input, ServerCheck, SessionPhase};

use crate::app_model::AppModel;
use crate::form::{self, ButtonKind};
use crate::states;
use crate::text_input::{TextInput, TextInputEvent, TextInputOptions};
use crate::theme::Theme;

/// What the last check of the server found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerStatus {
    /// Nothing has asked yet.
    Unchecked,
    Checking,
    Connected,
    /// The server speaks a newer protocol than this app.
    NeedsNewerLazy,
    /// The server speaks an older protocol than this app.
    NeedsOlderLazy,
    /// No answer, with the core's reason in a few words.
    Unreachable(String),
}

/// Everything the row shows, read from the core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerView {
    /// The address on screen: the one being tried when there is one, else the one in use.
    pub address: String,
    /// The address in use, when another is being tried.
    pub still_using: Option<String>,
    /// The address comes from the environment, so changing it here would not stick.
    pub locked: bool,
    /// Why the address cannot be changed right now (signed in; signing in).
    pub blocked: Option<&'static str>,
    /// Changing the server on this screen removes the saved sign-in of the one being left.
    pub removes_session: bool,
    /// What the core said about a refused change (unsent messages are waiting, and so on).
    pub notice: Option<String>,
    pub status: ServerStatus,
}

/// Reads the row's state from the core.
pub fn server_view(core: &ClientCore) -> ServerView {
    let server = core.server();
    let (blocked, removes_session) = match core.phase() {
        SessionPhase::SignedOut { .. } => (None, false),
        // A stored session that could not be loaded: moving away forgets it.
        SessionPhase::Unreachable { .. } => (None, true),
        SessionPhase::SigningIn => (Some(states::SERVER_WAIT_SIGN_IN), false),
        SessionPhase::Ready | SessionPhase::Loading => (Some(states::SERVER_SIGN_OUT_FIRST), false),
    };
    ServerView {
        address: server.trying.clone().unwrap_or_else(|| server.url.clone()),
        still_using: server.trying.as_ref().map(|_| server.url.clone()),
        locked: server.locked(),
        blocked,
        removes_session,
        notice: core.notice().map(str::to_owned),
        status: match &server.check {
            ServerCheck::Unchecked => ServerStatus::Unchecked,
            ServerCheck::Checking => ServerStatus::Checking,
            ServerCheck::Reachable { .. } => ServerStatus::Connected,
            ServerCheck::Incompatible { server, client } if server > client => {
                ServerStatus::NeedsNewerLazy
            }
            ServerCheck::Incompatible { .. } => ServerStatus::NeedsOlderLazy,
            ServerCheck::Unreachable(reason) => ServerStatus::Unreachable(reason.clone()),
        },
    }
}

pub struct ServerRow {
    model: Entity<AppModel>,
    input: Entity<TextInput>,
    editing: bool,
    /// The keyboard goes to "Change…" at the next render (it is not on screen while changing).
    refocus_change: bool,
    /// The status was asked for since it was last unknown, so it is not asked for again.
    asked: bool,
    change_focus: FocusHandle,
    check_focus: FocusHandle,
    keep_focus: FocusHandle,
    connect_focus: FocusHandle,
    cancel_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl ServerRow {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            TextInput::new(
                TextInputOptions::new(states::SERVER_PLACEHOLDER)
                    .accessible_label("Server address")
                    .single_line(true)
                    .max_bytes(512),
                cx,
            )
        });
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &input,
            window,
            |this, _input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => this.connect(window, cx),
                TextInputEvent::Tab { backwards } => {
                    if *backwards {
                        window.focus_prev(cx)
                    } else {
                        window.focus_next(cx)
                    }
                }
                TextInputEvent::Changed
                | TextInputEvent::UpWhenEmpty
                | TextInputEvent::PasteAttachments
                | TextInputEvent::Suggestion(_) => {}
            },
        ));
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        Self {
            model,
            input,
            editing: false,
            refocus_change: false,
            asked: false,
            change_focus: cx.focus_handle().tab_stop(true),
            check_focus: cx.focus_handle().tab_stop(true),
            keep_focus: cx.focus_handle().tab_stop(true),
            connect_focus: cx.focus_handle().tab_stop(true),
            cancel_focus: cx.focus_handle().tab_stop(true),
            _subscriptions: subscriptions,
        }
    }

    /// Whether the keyboard is in the row.
    pub fn has_focus(&self, window: &Window, cx: &gpui::App) -> bool {
        self.input.read(cx).is_focused(window)
            || [
                &self.change_focus,
                &self.check_focus,
                &self.keep_focus,
                &self.connect_focus,
                &self.cancel_focus,
            ]
            .iter()
            .any(|handle| handle.is_focused(window))
    }

    /// Whether the address is being changed. For tests.
    pub fn editing(&self) -> bool {
        self.editing
    }

    /// The field's focus handle. For tests.
    pub fn field_focus(&self, cx: &gpui::App) -> FocusHandle {
        self.input.focus_handle(cx)
    }

    /// The "Change…" button's focus handle. For tests.
    pub fn change_focus(&self) -> FocusHandle {
        self.change_focus.clone()
    }

    fn view(&self, cx: &gpui::App) -> ServerView {
        server_view(self.model.read(cx).core())
    }

    fn begin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.view(cx);
        if view.locked || view.blocked.is_some() || self.editing {
            return;
        }
        self.editing = true;
        self.input.update(cx, |input, cx| {
            input.set_text(&view.address, cx);
        });
        let field = self.input.focus_handle(cx);
        window.focus(&field, cx);
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if !self.editing {
            return;
        }
        self.editing = false;
        self.refocus_change = true;
        cx.notify();
    }

    /// Uses the address in the field and checks it.
    fn connect(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text().trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.editing = false;
        self.refocus_change = true;
        // The core makes it an address (or says why it cannot be one), checks it, and moves to it
        // when it is a Lazy server this app can talk to.
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::SetServerUrl(text), cx)
        });
        cx.notify();
    }

    /// Drops the address being tried: the core asks the one in use again.
    fn keep_current(&mut self, cx: &mut Context<Self>) {
        let url = self.model.read(cx).core().server().url.clone();
        self.model
            .update(cx, |model, cx| model.dispatch(Input::SetServerUrl(url), cx));
    }

    fn check_again(&mut self, cx: &mut Context<Self>) {
        self.model
            .update(cx, |model, cx| model.dispatch(Input::CheckServer, cx));
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editing || event.keystroke.key != "escape" {
            return;
        }
        // Escape belongs to an input method's composition while there is one.
        if crate::text_input::cancel_any_composition(window, cx) {
            cx.stop_propagation();
            return;
        }
        self.cancel(cx);
        cx.stop_propagation();
    }
}

/// The words for what a check found, and whether they are a problem.
pub fn status_text(status: &ServerStatus) -> Option<(String, Tone)> {
    match status {
        ServerStatus::Unchecked => None,
        ServerStatus::Checking => Some((states::SERVER_CHECKING.to_owned(), Tone::Neutral)),
        ServerStatus::Connected => Some((states::SERVER_CONNECTED.to_owned(), Tone::Good)),
        ServerStatus::NeedsNewerLazy => {
            Some((states::SERVER_NEEDS_NEWER.to_owned(), Tone::Warning))
        }
        ServerStatus::NeedsOlderLazy => {
            Some((states::SERVER_NEEDS_OLDER.to_owned(), Tone::Warning))
        }
        ServerStatus::Unreachable(reason) => Some((
            match reason.trim() {
                "" => states::SERVER_UNREACHABLE.to_owned(),
                // A refusal of what was typed is a sentence of its own.
                sentence if sentence.ends_with('.') => sentence.to_owned(),
                phrase => format!(
                    "{}: {phrase}.",
                    states::SERVER_UNREACHABLE.trim_end_matches('.')
                ),
            },
            Tone::Bad,
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Good,
    Warning,
    Bad,
}

impl Render for ServerRow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        let view = self.view(cx);
        if self.refocus_change && !self.editing {
            self.refocus_change = false;
            if !view.locked && view.blocked.is_none() {
                window.focus(&self.change_focus, cx);
            }
        }
        // Say what the server is when the screen first shows it, so "Connected" is there before
        // anyone asks. Once per time the answer was unknown.
        if view.status == ServerStatus::Unchecked && !self.asked && view.blocked.is_none() {
            self.asked = true;
            cx.defer_in(window, |this, _window, cx| this.check_again(cx));
        } else if view.status != ServerStatus::Unchecked {
            self.asked = false;
        }
        let this = cx.entity().downgrade();
        let change_to = this.clone();
        let check_to = this.clone();
        let keep_to = this.clone();
        let connect_to = this.clone();
        let cancel_to = this.clone();
        let status = status_text(&view.status);
        let tone_color = |tone: Tone| match tone {
            Tone::Neutral => colors.text_muted,
            Tone::Good => colors.success,
            Tone::Warning => colors.warning,
            Tone::Bad => colors.danger,
        };
        let focused = self.input.read(cx).is_focused(window);
        let mut row =
            div()
                .id("server-row")
                .debug_selector(|| "server-row".into())
                .role(Role::Group)
                .flex()
                .flex_col()
                .gap_1()
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    this.on_key(event, window, cx)
                }))
                .child(
                    div()
                        .text_size(theme.type_scale.small)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.text_muted)
                        .child("Server"),
                );
        row = if self.editing {
            row.child(
                div()
                    .debug_selector(|| "server-field".into())
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
                    .child(self.input.clone()),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(form::button(
                        &theme,
                        "server-connect",
                        "Connect",
                        ButtonKind::Primary,
                        true,
                        &self.connect_focus,
                        move |window, cx| {
                            connect_to
                                .update(cx, |row, cx| row.connect(window, cx))
                                .ok();
                        },
                    ))
                    .child(form::button(
                        &theme,
                        "server-cancel",
                        "Cancel",
                        ButtonKind::Secondary,
                        true,
                        &self.cancel_focus,
                        move |_, cx| {
                            cancel_to.update(cx, |row, cx| row.cancel(cx)).ok();
                        },
                    )),
            )
        } else {
            row.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .id("server-address")
                            .debug_selector(|| "server-address".into())
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(theme.type_scale.body)
                            .text_color(colors.text_muted)
                            .child(view.address.clone()),
                    )
                    .children((!view.locked).then(|| {
                        form::button(
                            &theme,
                            "server-change",
                            "Change…",
                            ButtonKind::Secondary,
                            view.blocked.is_none(),
                            &self.change_focus,
                            move |window, cx| {
                                change_to.update(cx, |row, cx| row.begin(window, cx)).ok();
                            },
                        )
                    })),
            )
            .children(view.locked.then(|| {
                div()
                    .debug_selector(|| "server-locked".into())
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_faint)
                    .child(states::SERVER_LOCKED)
            }))
            .children(view.blocked.filter(|_| !view.locked).map(|reason| {
                div()
                    .debug_selector(|| "server-blocked".into())
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_faint)
                    .child(reason)
            }))
        };
        if self.editing && view.removes_session {
            row = row.child(
                div()
                    .debug_selector(|| "server-removes-session".into())
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_faint)
                    .child(states::SERVER_REMOVES_SESSION),
            );
        }
        let can_check_again = !self.editing
            && matches!(
                view.status,
                ServerStatus::Unreachable(_)
                    | ServerStatus::NeedsNewerLazy
                    | ServerStatus::NeedsOlderLazy
            );
        row.children(status.map(|(text, tone)| {
            div()
                .id("server-status")
                .debug_selector(|| "server-status".into())
                .role(Role::Status)
                .text_size(theme.type_scale.small)
                .text_color(tone_color(tone))
                .child(text)
        }))
        .children(view.notice.filter(|_| !self.editing).map(|notice| {
            div()
                .id("server-notice")
                .debug_selector(|| "server-notice".into())
                .text_size(theme.type_scale.small)
                .text_color(colors.text_muted)
                .child(notice)
        }))
        .children(
            view.still_using
                .clone()
                .filter(|_| !self.editing)
                .map(|url| {
                    div()
                        .debug_selector(|| "server-still-using".into())
                        .truncate()
                        .text_size(theme.type_scale.small)
                        .text_color(colors.text_faint)
                        .child(format!("Still using {url}"))
                }),
        )
        .children(can_check_again.then(|| {
            div()
                .flex()
                .gap_2()
                .child(form::button(
                    &theme,
                    "server-check",
                    "Check again",
                    ButtonKind::Secondary,
                    view.blocked.is_none() || view.locked,
                    &self.check_focus,
                    move |_, cx| {
                        check_to.update(cx, |row, cx| row.check_again(cx)).ok();
                    },
                ))
                .children(
                    (view.still_using.is_some() && view.blocked.is_none()).then(|| {
                        form::button(
                            &theme,
                            "server-keep",
                            "Keep the current server",
                            ButtonKind::Secondary,
                            true,
                            &self.keep_focus,
                            move |_, cx| {
                                keep_to.update(cx, |row, cx| row.keep_current(cx)).ok();
                            },
                        )
                    }),
                )
        }))
        .child(div().h(px(0.)))
    }
}
