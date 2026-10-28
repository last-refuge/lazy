//! The window's root: chooses what to show for the current session phase.

use gpui::{
    AnyElement, Context, Entity, FontWeight, IntoElement, Render, Window, div, prelude::*, px,
};
use lazy_client_core::{Input, SessionPhase};

use crate::app_model::AppModel;
use crate::fonts;
use crate::shell::ShellView;
use crate::sign_in::SignInView;
use crate::theme::Theme;

pub struct RootView {
    model: Entity<AppModel>,
    sign_in: Entity<SignInView>,
    shell: Option<Entity<ShellView>>,
}

impl RootView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sign_in = cx.new(|cx| SignInView::new(model.clone(), window, cx));
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        Self {
            model,
            sign_in,
            shell: None,
        }
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
                    .text_size(theme.type_scale.body)
                    .text_color(theme.colors.text_muted)
                    .child(detail)
            }))
    }

    fn unreachable_screen(
        &self,
        theme: &Theme,
        message: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        Self::splash(theme, "Can't reach the server", Some(message))
            .child(
                div()
                    .id("retry-load")
                    .mt_3()
                    .px_4()
                    .py_2()
                    .rounded(theme.radii.medium)
                    .cursor_pointer()
                    .bg(theme.colors.accent)
                    .text_color(theme.colors.on_accent)
                    .font_weight(FontWeight::SEMIBOLD)
                    .hover(|style| style.bg(theme.colors.accent_hover))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.model
                            .update(cx, |model, cx| model.dispatch(Input::RetryLoad, cx));
                    }))
                    .child("Try again"),
            )
            .into_any_element()
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let phase = self.model.read(cx).core().phase().clone();

        if !matches!(phase, SessionPhase::Ready) {
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
            SessionPhase::Unreachable { message } => self.unreachable_screen(&theme, message, cx),
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
