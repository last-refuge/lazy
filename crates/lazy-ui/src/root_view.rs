//! The window's root: chooses what to show for the current session phase.

use gpui::{
    AnyElement, Context, Entity, FocusHandle, FontWeight, IntoElement, Render, Window, div,
    prelude::*, px,
};
use lazy_client_core::{Input, SessionPhase};

use crate::app_model::AppModel;
use crate::fonts;
use crate::form::{self, ButtonKind};
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
}

impl RootView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // How this device looks and moves, as it was saved.
        let appearance = model.read(cx).core().appearance();
        crate::theme::apply_appearance(cx, appearance);
        let sign_in = cx.new(|cx| SignInView::new(model.clone(), window, cx));
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
        }
    }

    /// The signed-in shell, once there is one.
    pub fn shell(&self) -> Option<&Entity<ShellView>> {
        self.shell.as_ref()
    }

    fn splash(theme: &Theme, headline: &str, detail: Option<String>) -> gpui::Div {
        div()
            .size_full()
            .bg(theme.colors.surface)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                div()
                    .text_size(theme.type_scale.heading)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.colors.text)
                    .child(headline.to_owned()),
            )
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
        Self::splash(theme, &title, Some(detail.to_owned()))
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
            .into_any_element()
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
        Self::splash(theme, "Can't reach the server", Some(message))
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
                Self::splash(&theme, "Lazy", Some("Loading…".into())).into_any_element()
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
