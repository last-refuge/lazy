//! The window's root: chooses what to show for the current session phase.

use gpui::{
    AnyElement, Context, Entity, FocusHandle, IntoElement, Render, Window, div, prelude::*, px,
};
use lazy_client_core::{Input, SessionPhase};

use crate::app_model::AppModel;
use crate::fonts;
use crate::form::{self, ButtonKind};
use crate::server_row::ServerRow;
use crate::shell::ShellView;
use crate::sign_in::SignInView;
use crate::theme::Theme;

pub struct RootView {
    model: Entity<AppModel>,
    sign_in: Entity<SignInView>,
    shell: Option<Entity<ShellView>>,
    /// The "Try again" button of the unreachable-server screen, which has the keyboard when that
    /// screen appears.
    retry_focus: FocusHandle,
    /// The unreachable screen is showing, and its button has already been given the keyboard.
    retry_focused: bool,
    /// The no-access screen's buttons: "Check again" has the keyboard when it appears.
    check_focus: FocusHandle,
    sign_out_focus: FocusHandle,
    sign_out_focused: bool,
    /// The server row of the unreachable and no-access screens (D25).
    server_row: Entity<ServerRow>,
}

impl RootView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // How this device looks and moves, as it was saved.
        let appearance = model.read(cx).core().appearance();
        crate::theme::apply_appearance(cx, appearance);
        let sign_in = cx.new(|cx| SignInView::new(model.clone(), window, cx));
        let server_row = cx.new(|cx| ServerRow::new(model.clone(), window, cx));
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        Self {
            model,
            sign_in,
            shell: None,
            retry_focus: cx.focus_handle().tab_stop(true),
            retry_focused: false,
            check_focus: cx.focus_handle().tab_stop(true),
            sign_out_focus: cx.focus_handle().tab_stop(true),
            sign_out_focused: false,
            server_row,
        }
    }

    /// The "Server" row of the screen showing it: the form's, the unreachable screen's, or the
    /// no-access screen's. For tests.
    pub fn server_row(&self, cx: &gpui::App) -> Entity<ServerRow> {
        match self.model.read(cx).core().phase() {
            SessionPhase::SignedOut { .. } | SessionPhase::SigningIn => {
                self.sign_in.read(cx).server_row().clone()
            }
            _ => self.server_row.clone(),
        }
    }

    /// The signed-in shell, once there is one.
    pub fn shell(&self) -> Option<&Entity<ShellView>> {
        self.shell.as_ref()
    }

    /// A screen that is one message: the logo, a heading (in the brand's heading face), and a line
    /// under it. Moss is not here: these are loading, outage, and access screens.
    fn splash(
        theme: &Theme,
        logo: Option<gpui::Stateful<gpui::Div>>,
        headline: &str,
        detail: Option<String>,
    ) -> gpui::Div {
        div()
            .size_full()
            .bg(theme.colors.surface)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .children(logo.map(|logo| div().pb_4().child(logo)))
            .child(crate::fonts::heading(
                div()
                    .text_size(theme.type_scale.heading)
                    .text_color(theme.colors.text)
                    .child(headline.to_owned()),
            ))
            .children(detail.map(|detail| {
                div()
                    .max_w(px(420.))
                    .text_center()
                    .text_size(theme.type_scale.body)
                    .text_color(theme.colors.text_muted)
                    .child(detail)
            }))
    }

    /// Shown to a person who has no workspace: their membership was deactivated (the core knows
    /// which workspace it was while the run lasts) or the server lists none for them. "Check
    /// again" asks for a bootstrap, so a person an admin has let back in does not have to sign
    /// out; Sign out is the other way out. Their account is fine either way.
    fn no_access_screen(
        &mut self,
        theme: &Theme,
        workspace: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.sign_out_focused {
            self.sign_out_focused = true;
            window.focus(&self.check_focus, cx);
        }
        let check_to = cx.entity().downgrade();
        let this = cx.entity().downgrade();
        let (title, detail) = match &workspace {
            Some(name) => (
                crate::states::no_access_title(name),
                crate::states::NO_ACCESS_DETAIL,
            ),
            None => (
                crate::states::NO_WORKSPACE_TITLE.to_owned(),
                crate::states::NO_WORKSPACE_DETAIL,
            ),
        };
        Self::splash(
            theme,
            crate::brand::logo(cx, 160.),
            &title,
            Some(detail.to_owned()),
        )
            .id("no-access")
            .debug_selector(|| "no-access".into())
            // Tab moves between the two buttons, as it does on the sign-in form.
            .key_context(crate::keyboard::SHELL_CONTEXT)
            .on_action(
                cx.listener(|_, _: &crate::keyboard::FocusNextControl, window, cx| {
                    window.focus_next(cx)
                }),
            )
            .on_action(
                cx.listener(|_, _: &crate::keyboard::FocusPrevControl, window, cx| {
                    window.focus_prev(cx)
                }),
            )
            .child(
                div()
                    .mt_3()
                    .flex()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "no-access-check",
                        "Check again",
                        ButtonKind::Primary,
                        true,
                        &self.check_focus,
                        move |_, cx| {
                            check_to
                                .update(cx, |root, cx| {
                                    root.model.update(cx, |model, cx| {
                                        model.dispatch(Input::RetryLoad, cx)
                                    });
                                })
                                .ok();
                        },
                    ))
                    .child(form::button(
                        theme,
                        "no-access-sign-out",
                        "Sign out",
                        ButtonKind::Secondary,
                        true,
                        &self.sign_out_focus,
                        move |_, cx| {
                            this.update(cx, |root, cx| {
                                root.model
                                    .update(cx, |model, cx| model.dispatch(Input::SignOut, cx));
                            })
                            .ok();
                        },
                    )),
            )
            .child(self.server_section())
            .into_any_element()
    }

    /// The "Server" row under the buttons of the unreachable and no-access screens.
    fn server_section(&self) -> gpui::Div {
        div().mt_6().w(px(360.)).child(self.server_row.clone())
    }

    fn unreachable_screen(
        &mut self,
        theme: &Theme,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The one thing to do here is the first thing the keyboard reaches.
        if !self.retry_focused {
            self.retry_focused = true;
            window.focus(&self.retry_focus, cx);
        }
        let this = cx.entity().downgrade();
        Self::splash(
            theme,
            crate::brand::logo(cx, 160.),
            "Can't reach the server",
            Some(message),
        )
            // Tab moves between "Try again" and the server row, as it does on the sign-in form.
            .key_context(crate::keyboard::SHELL_CONTEXT)
            .on_action(
                cx.listener(|_, _: &crate::keyboard::FocusNextControl, window, cx| {
                    window.focus_next(cx)
                }),
            )
            .on_action(
                cx.listener(|_, _: &crate::keyboard::FocusPrevControl, window, cx| {
                    window.focus_prev(cx)
                }),
            )
            .child(div().mt_3().child(form::button(
                theme,
                "retry-load",
                "Try again",
                ButtonKind::Primary,
                true,
                &self.retry_focus,
                move |_, cx| {
                    this.update(cx, |root, cx| {
                        root.model
                            .update(cx, |model, cx| model.dispatch(Input::RetryLoad, cx));
                    })
                    .ok();
                },
            )))
            .child(self.server_section())
            .into_any_element()
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let phase = self.model.read(cx).core().phase().clone();

        if !matches!(phase, SessionPhase::Unreachable { .. }) {
            self.retry_focused = false;
        }
        // A ready session with no workspace has nothing to show but why: the name when the core
        // knows which workspace it lost, else a generic line.
        let no_workspace = matches!(phase, SessionPhase::Ready) && {
            let core = self.model.read(cx).core();
            core.primary_workspace().is_none()
        };
        let access_lost: Option<Option<String>> =
            no_workspace.then(|| self.model.read(cx).core().access_lost().map(str::to_owned));
        if access_lost.is_none() {
            self.sign_out_focused = false;
        }
        if !matches!(phase, SessionPhase::Ready) || access_lost.is_some() {
            // Leaving the ready state (sign-out, expired session) discards the shell, so its
            // timelines and composer start fresh for the next session.
            self.shell = None;
        }

        let content = match phase {
            SessionPhase::Loading => {
                Self::splash(
                    &theme,
                    crate::brand::logo(cx, 160.),
                    "Lazy",
                    Some("Loading…".into()),
                )
                .into_any_element()
            }
            SessionPhase::SignedOut { .. } | SessionPhase::SigningIn => {
                self.sign_in.clone().into_any_element()
            }
            SessionPhase::Unreachable { message } => {
                self.unreachable_screen(&theme, message, window, cx)
            }
            SessionPhase::Ready if access_lost.is_some() => {
                self.no_access_screen(&theme, access_lost.flatten(), window, cx)
            }
            SessionPhase::Ready => {
                let model = self.model.clone();
                let shell = self
                    .shell
                    .get_or_insert_with(|| cx.new(|cx| ShellView::new(model, window, cx)));
                shell.clone().into_any_element()
            }
        };
        div().size_full().font_family(fonts::FAMILY).child(content)
    }
}
