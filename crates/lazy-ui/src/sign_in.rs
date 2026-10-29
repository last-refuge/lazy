//! Sign-in and account creation.

use gpui::{
    AnyElement, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement, Render,
    ScrollHandle, SharedString, Subscription, Window, div, prelude::*, px,
};
use lazy_client_core::{Input, SessionPhase};

use crate::app_model::AppModel;
use crate::keyboard::{self, FocusNextControl, FocusPrevControl};
use crate::server_row::ServerRow;
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
    /// The code of an invitation, optional, when creating an account.
    invite_code: Entity<TextInput>,
    submit_focus: FocusHandle,
    toggle_focus: FocusHandle,
    /// Which server this is, and a way to change it; its stops come after the form's own.
    server_row: Entity<ServerRow>,
    /// The card scrolls when the window is too short for it, and follows the keyboard.
    scroll: ScrollHandle,
    scrolled_for: Option<usize>,
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
        // A code is a secret like a password: shown as typed here only while the person looks, but
        // never kept in a log or a label (its field is labelled by its name alone).
        let invite_code = field(
            crate::states::INVITE_CODE_PLACEHOLDER,
            "Invitation or setup code",
            false,
            cx,
        );

        let mut subscriptions = Vec::new();
        for input in [&username, &display_name, &password, &invite_code] {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |this, _input, event: &TextInputEvent, window, cx| match event {
                    TextInputEvent::Submit => this.submit(cx),
                    TextInputEvent::Tab { backwards } => this.move_focus(*backwards, window, cx),
                    TextInputEvent::Changed
                    | TextInputEvent::UpWhenEmpty
                    | TextInputEvent::PasteAttachments
                    | TextInputEvent::Suggestion(_) => {}
                },
            ));
        }
        let handle = username.focus_handle(cx);
        window.focus(&handle, cx);
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        let server_row = cx.new(|cx| ServerRow::new(model.clone(), window, cx));

        Self {
            model,
            mode: Mode::SignIn,
            username,
            display_name,
            password,
            invite_code,
            submit_focus: cx.focus_handle().tab_stop(true),
            toggle_focus: cx.focus_handle().tab_stop(true),
            server_row,
            scroll: ScrollHandle::new(),
            scrolled_for: None,
            _subscriptions: subscriptions,
        }
    }

    /// The "Server" row. For tests.
    pub fn server_row(&self) -> &Entity<ServerRow> {
        &self.server_row
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
                invite_code: self.invite_code.read(cx).text().trim().to_owned(),
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

        // Which row of the card has the keyboard, to keep it in view in a short window.
        let mut focused_row: Option<usize> = None;
        let mut row = 1usize;
        let mut note = |has: bool| {
            if has {
                focused_row = Some(row);
            }
            row += 1;
        };
        let max_height = (f32::from(window.viewport_size().height) - 32.0).max(200.0);
        let mut form = div()
            .id("sign-in-card")
            .debug_selector(|| "sign-in-card".into())
            .max_h(px(max_height))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .w(px(360.))
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .rounded(theme.radii.large)
            .bg(theme.colors.surface_raised)
            .border_1()
            .border_color(theme.colors.border)
            // The logo, the headline, and a line under it: the brand's opening (one row of the
            // card, so the rows below keep their numbers).
            .child(crate::brand::masthead(
                cx,
                Some(if creating {
                    "Create your account"
                } else {
                    crate::states::BRAND_SUPPORT
                }),
            ))
            .child(self.labeled(&theme, "Username", &self.username, window, cx));
        note(self.username.read(cx).is_focused(window));
        if creating {
            form = form.child(self.labeled(&theme, "Display name", &self.display_name, window, cx));
            note(self.display_name.read(cx).is_focused(window));
        }
        form = form.child(self.labeled(&theme, "Password", &self.password, window, cx));
        note(self.password.read(cx).is_focused(window));
        if creating {
            note(self.invite_code.read(cx).is_focused(window));
            form = form.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(self.labeled(
                        &theme,
                        crate::states::INVITE_CODE_LABEL,
                        &self.invite_code,
                        window,
                        cx,
                    ))
                    .child(
                        div()
                            .debug_selector(|| "register-code-hint".into())
                            .text_size(theme.type_scale.small)
                            .text_color(theme.colors.text_faint)
                            .child(crate::states::INVITE_CODE_HINT),
                    ),
            );
        }
        if error.is_some() {
            note(false);
        }
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
        note(self.submit_focus.is_focused(window));
        note(self.toggle_focus.is_focused(window));
        note(self.server_row.read(cx).has_focus(window, cx));
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
            )
            // Last, so the form's own tab order is what it always was.
            .child(
                div()
                    .pt_4()
                    .border_t_1()
                    .border_color(theme.colors.border)
                    .child(self.server_row.clone()),
            );

        if focused_row != self.scrolled_for {
            self.scrolled_for = focused_row;
            if let Some(row) = focused_row
                && window.last_input_was_keyboard()
            {
                self.scroll.scroll_to_item(row);
            }
        }
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
