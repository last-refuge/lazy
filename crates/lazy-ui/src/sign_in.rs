//! Sign-in and account creation.

use gpui::{
    AnyElement, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement, Render,
    SharedString, Subscription, Window, div, prelude::*, px,
};
use lazy_client_core::{Input, SessionPhase};

use crate::app_model::AppModel;
use crate::keyboard::{self, FocusNextControl, FocusPrevControl};
use crate::text_input::{TextInput, TextInputEvent, TextInputOptions};
use crate::theme::Theme;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    SignIn,
    CreateAccount,
}

pub struct SignInView {
    model: Entity<AppModel>,
    mode: Mode,
    username: Entity<TextInput>,
    display_name: Entity<TextInput>,
    password: Entity<TextInput>,
    submit_focus: FocusHandle,
    toggle_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl SignInView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let field = |placeholder: &'static str,
                     label: &'static str,
                     masked: bool,
                     cx: &mut Context<Self>| {
            cx.new(|cx| {
                TextInput::new(
                    TextInputOptions::new(placeholder)
                        .accessible_label(label)
                        .single_line(true)
                        .masked(masked)
                        .max_bytes(256),
                    cx,
                )
            })
        };
        let username = field("Username", "Username", false, cx);
        let display_name = field("Display name", "Display name", false, cx);
        let password = field("Password", "Password", true, cx);

        let mut subscriptions = Vec::new();
        for input in [&username, &display_name, &password] {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |this, _input, event: &TextInputEvent, window, cx| match event {
                    TextInputEvent::Submit => this.submit(cx),
                    TextInputEvent::Tab { backwards } => this.move_focus(*backwards, window, cx),
                    TextInputEvent::Changed => {}
                },
            ));
        }
        let handle = username.focus_handle(cx);
        window.focus(&handle, cx);
        cx.observe(&model, |_, _, cx| cx.notify()).detach();

        Self {
            model,
            mode: Mode::SignIn,
            username,
            display_name,
            password,
            submit_focus: cx.focus_handle().tab_stop(true),
            toggle_focus: cx.focus_handle().tab_stop(true),
            _subscriptions: subscriptions,
        }
    }

    /// Tab and Shift+Tab leave a single-line field for the next or previous control, which may be
    /// another field or one of the form's buttons.
    fn move_focus(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        if backwards {
            window.focus_prev(cx);
        } else {
            window.focus_next(cx);
        }
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.mode = mode;
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if matches!(self.model.read(cx).core().phase(), SessionPhase::SigningIn) {
            return;
        }
        let username = self.username.read(cx).text().to_owned();
        let password = self.password.read(cx).text().to_owned();
        let input = match self.mode {
            Mode::SignIn => Input::SignIn { username, password },
            Mode::CreateAccount => Input::Register {
                username,
                display_name: self.display_name.read(cx).text().to_owned(),
                password,
            },
        };
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
    }

    fn labeled(
        &self,
        theme: &Theme,
        label: &'static str,
        input: &Entity<TextInput>,
        window: &Window,
        cx: &gpui::App,
    ) -> AnyElement {
        let focused = input.read(cx).is_focused(window);
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(theme.type_scale.small)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.colors.text_muted)
                    .child(label),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded(theme.radii.medium)
                    .bg(theme.colors.surface_raised)
                    .border_1()
                    .border_color(if focused {
                        theme.colors.focus_ring
                    } else {
                        theme.colors.border_strong
                    })
                    .child(input.clone()),
            )
            .into_any_element()
    }
}

impl Render for SignInView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let (busy, error) = match self.model.read(cx).core().phase() {
            SessionPhase::SigningIn => (true, None),
            SessionPhase::SignedOut { last_error } => (false, last_error.clone()),
            _ => (false, None),
        };
        let creating = self.mode == Mode::CreateAccount;
        let busy_label: SharedString = if creating {
            "Creating account…"
        } else {
            "Signing in…"
        }
        .into();
        let action_label: SharedString = if creating {
            "Create account"
        } else {
            "Sign in"
        }
        .into();

        let mut form = div()
            .w(px(360.))
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .rounded(theme.radii.large)
            .bg(theme.colors.surface_raised)
            .border_1()
            .border_color(theme.colors.border)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(theme.type_scale.heading)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.colors.text)
                            .child("Lazy"),
                    )
                    .child(
                        div()
                            .text_size(theme.type_scale.body)
                            .text_color(theme.colors.text_muted)
                            .child(if creating {
                                "Create your account"
                            } else {
                                "Sign in to your workspace"
                            }),
                    ),
            )
            .child(self.labeled(&theme, "Username", &self.username, window, cx));
        if creating {
            form = form.child(self.labeled(&theme, "Display name", &self.display_name, window, cx));
        }
        form = form.child(self.labeled(&theme, "Password", &self.password, window, cx));
        if let Some(error) = error {
            form = form.child(
                div()
                    .px_3()
                    .py_2()
                    .rounded(theme.radii.small)
                    .bg(theme.colors.danger_surface)
                    .text_size(theme.type_scale.small)
                    .text_color(theme.colors.danger)
                    .child(error),
            );
        }
        form = form
            .child(
                div()
                    .id("sign-in-submit")
                    .debug_selector(|| "sign-in-submit".into())
                    .track_focus(&self.submit_focus)
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .focus_visible(|style| style.border_color(theme.colors.focus_ring))
                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                        if keyboard::is_activation(event) {
                            this.submit(cx);
                            cx.stop_propagation();
                        }
                    }))
                    .flex()
                    .items_center()
                    .justify_center()
                    .py_2()
                    .rounded(theme.radii.medium)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(theme.type_scale.body)
                    .when(!busy, |button| {
                        button
                            .cursor_pointer()
                            .bg(theme.colors.accent)
                            .text_color(theme.colors.on_accent)
                            .hover(|style| style.bg(theme.colors.accent_hover))
                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx)))
                    })
                    .when(busy, |button| {
                        button
                            .bg(theme.colors.surface_hover)
                            .text_color(theme.colors.text_muted)
                    })
                    .child(if busy { busy_label } else { action_label }),
            )
            .child(
                div()
                    .id("sign-in-toggle-mode")
                    .debug_selector(|| "sign-in-toggle-mode".into())
                    .track_focus(&self.toggle_focus)
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .focus_visible(|style| style.border_color(theme.colors.focus_ring))
                    .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                        if keyboard::is_activation(event) {
                            this.set_mode(
                                if creating {
                                    Mode::SignIn
                                } else {
                                    Mode::CreateAccount
                                },
                                cx,
                            );
                            cx.stop_propagation();
                        }
                    }))
                    .flex()
                    .justify_center()
                    .cursor_pointer()
                    .text_size(theme.type_scale.small)
                    .text_color(theme.colors.accent)
                    .hover(|style| style.text_color(theme.colors.accent_hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_mode(
                            if creating {
                                Mode::SignIn
                            } else {
                                Mode::CreateAccount
                            },
                            cx,
                        );
                    }))
                    .child(if creating {
                        "Have an account? Sign in"
                    } else {
                        "New here? Create an account"
                    }),
            );

        div()
            .size_full()
            .key_context(keyboard::SHELL_CONTEXT)
            .on_action(cx.listener(|_, _: &FocusNextControl, window, cx| window.focus_next(cx)))
            .on_action(cx.listener(|_, _: &FocusPrevControl, window, cx| window.focus_prev(cx)))
            .bg(theme.colors.surface)
            .flex()
            .items_center()
            .justify_center()
            .child(form)
    }
}
