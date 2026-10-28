//! The signed-in layout: workspace rail, conversation sidebar, and the conversation pane.

use std::collections::HashMap;

use gpui::{
    AnyElement, Context, Entity, FocusHandle, FontWeight, IntoElement, ParentElement, Render,
    SharedString, Styled, Window, div, prelude::*, px,
};
use lazy_client_core::{ConnectionState, Input};
use lazy_domain::ConversationId;

use crate::app_model::AppModel;
use crate::composer::Composer;
use crate::keyboard::{self, FocusNextControl, FocusPrevControl};
use crate::theme::{Theme, avatar_color};
use crate::timeline::TimelineView;

const RAIL_WIDTH: f32 = 64.0;
const SIDEBAR_WIDTH: f32 = 260.0;
/// Below this window width the sidebar narrows and the workspace rail is hidden.
const COMPACT_BELOW: f32 = 760.0;
const COMPACT_SIDEBAR_WIDTH: f32 = 208.0;

pub struct ShellView {
    model: Entity<AppModel>,
    timeline: Entity<TimelineView>,
    composer: Entity<Composer>,
    /// One tab stop per sidebar channel, created when the channel first appears.
    channel_focus: HashMap<ConversationId, FocusHandle>,
    sign_out_focus: FocusHandle,
}

impl ShellView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let timeline = cx.new(|cx| TimelineView::new(model.clone(), cx));
        let composer = cx.new(|cx| Composer::new(model.clone(), window, cx));
        // Re-render when the core changes so the sidebar, header, and banners stay current.
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        Self {
            model,
            timeline,
            composer,
            channel_focus: HashMap::new(),
            sign_out_focus: cx.focus_handle().tab_stop(true),
        }
    }

    fn select(&mut self, id: ConversationId, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::SelectConversation(id), cx)
        });
    }

    fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.model
            .update(cx, |model, cx| model.dispatch(Input::SignOut, cx));
    }

    fn dismiss_notice(&mut self, cx: &mut Context<Self>) {
        self.model
            .update(cx, |model, cx| model.dispatch(Input::DismissNotice, cx));
    }

    fn rail(&self, theme: &Theme, workspace_name: &str) -> AnyElement {
        let initial = workspace_name
            .chars()
            .next()
            .unwrap_or('L')
            .to_uppercase()
            .to_string();
        div()
            .w(px(RAIL_WIDTH))
            .flex_none()
            .h_full()
            .bg(theme.colors.rail)
            .flex()
            .flex_col()
            .items_center()
            .pt_3()
            .child(
                div()
                    .size(px(40.))
                    .rounded(theme.radii.large)
                    .bg(theme.colors.accent)
                    .text_color(theme.colors.on_accent)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(theme.type_scale.title)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(initial),
            )
            .into_any_element()
    }

    fn sidebar(&mut self, theme: &Theme, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let core = self.model.read(cx).core();
        let workspace_name = core
            .primary_workspace()
            .map_or_else(|| "Lazy".to_owned(), |info| info.workspace.name.clone());
        let me_name = core
            .me()
            .map(|user| user.display_name.to_string())
            .unwrap_or_default();
        let me_seed = core.me().map_or(0, |user| user.id.as_uuid().as_u128());
        let selected = core.selected_conversation();
        let mut channels: Vec<_> = core
            .conversations()
            .iter()
            .map(|conversation| (conversation.id, conversation.name.to_string()))
            .collect();
        channels.sort_by(|a, b| a.1.cmp(&b.1));

        let mut list = div().flex().flex_col().gap_0p5().px_2();
        for (id, name) in channels {
            let is_selected = selected == Some(id);
            let focus = self
                .channel_focus
                .entry(id)
                .or_insert_with(|| cx.focus_handle().tab_stop(true))
                .clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("channel-{id}")))
                    .debug_selector(|| format!("channel-{name}"))
                    .track_focus(&focus)
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .focus_visible(|style| style.border_color(theme.colors.focus_ring))
                    .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                        if keyboard::is_activation(event) {
                            this.select(id, cx);
                            cx.stop_propagation();
                        }
                    }))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(theme.radii.small)
                    .cursor_pointer()
                    .text_size(theme.type_scale.body)
                    .when(is_selected, |row| {
                        row.bg(theme.colors.sidebar_selected)
                            .text_color(theme.colors.sidebar_selected_text)
                            .font_weight(FontWeight::SEMIBOLD)
                    })
                    .when(!is_selected, |row| {
                        row.text_color(theme.colors.sidebar_text)
                            .hover(|style| style.bg(theme.colors.sidebar_hover))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.select(id, cx)))
                    .child(div().w(px(14.)).child("#"))
                    .child(div().truncate().child(name)),
            );
        }

        div()
            .w(px(width))
            .flex_none()
            .h_full()
            .bg(theme.colors.sidebar)
            .border_r_1()
            .border_color(theme.colors.border)
            .flex()
            .flex_col()
            .child(
                div()
                    .px_4()
                    .h(px(52.))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.colors.border)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(theme.type_scale.title)
                    .text_color(theme.colors.sidebar_text)
                    .child(div().truncate().child(workspace_name)),
            )
            .child(
                div()
                    .px_4()
                    .pt_3()
                    .pb_1()
                    .text_size(theme.type_scale.small)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.colors.sidebar_text_muted)
                    .child("Channels"),
            )
            .child(div().flex_1().overflow_hidden().child(list))
            .child(
                div()
                    .px_3()
                    .py_3()
                    .border_t_1()
                    .border_color(theme.colors.border)
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .size(px(28.))
                            .flex_none()
                            .rounded(theme.radii.small)
                            .bg(avatar_color(me_seed))
                            .text_color(gpui::white())
                            .font_weight(FontWeight::SEMIBOLD)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                me_name
                                    .chars()
                                    .next()
                                    .unwrap_or('?')
                                    .to_uppercase()
                                    .to_string(),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(theme.type_scale.body)
                            .text_color(theme.colors.sidebar_text)
                            .child(me_name),
                    )
                    .child(
                        div()
                            .id("sign-out")
                            .debug_selector(|| "sign-out".into())
                            .track_focus(&self.sign_out_focus)
                            .border_1()
                            .border_color(gpui::transparent_black())
                            .focus_visible(|style| style.border_color(theme.colors.focus_ring))
                            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                                if keyboard::is_activation(event) {
                                    this.sign_out(cx);
                                    cx.stop_propagation();
                                }
                            }))
                            .cursor_pointer()
                            .px_2()
                            .py_0p5()
                            .rounded(theme.radii.small)
                            .text_size(theme.type_scale.small)
                            .text_color(theme.colors.sidebar_text_muted)
                            .hover(|style| {
                                style
                                    .bg(theme.colors.sidebar_hover)
                                    .text_color(theme.colors.sidebar_text)
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.sign_out(cx)))
                            .child("Sign out"),
                    ),
            )
            .into_any_element()
    }

    fn banner(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let core = self.model.read(cx).core();
        let connection = match core.connection() {
            ConnectionState::Reconnecting { attempt } => Some((
                format!("Connection lost. Reconnecting… (attempt {attempt})"),
                true,
            )),
            ConnectionState::Connecting => Some(("Connecting…".to_owned(), false)),
            ConnectionState::CatchingUp => Some(("Catching up…".to_owned(), false)),
            ConnectionState::Live | ConnectionState::Offline => None,
        };
        let notice = core.notice().map(str::to_owned);

        if connection.is_none() && notice.is_none() {
            return None;
        }
        let mut stack = div().flex().flex_col();
        if let Some((text, is_warning)) = connection {
            stack = stack.child(
                div()
                    .px_4()
                    .py_1()
                    .text_size(theme.type_scale.small)
                    .bg(if is_warning {
                        theme.colors.warning_surface
                    } else {
                        theme.colors.surface_hover
                    })
                    .text_color(if is_warning {
                        theme.colors.warning
                    } else {
                        theme.colors.text_muted
                    })
                    .child(text),
            );
        }
        if let Some(text) = notice {
            stack = stack.child(
                div()
                    .px_4()
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_size(theme.type_scale.small)
                    .bg(theme.colors.danger_surface)
                    .text_color(theme.colors.danger)
                    .child(div().flex_1().child(text))
                    .child(
                        div()
                            .id("dismiss-notice")
                            .cursor_pointer()
                            .px_2()
                            .rounded(theme.radii.small)
                            .hover(|style| style.bg(theme.colors.surface_hover))
                            .on_click(cx.listener(|this, _, _, cx| this.dismiss_notice(cx)))
                            .child("Dismiss"),
                    ),
            );
        }
        Some(stack.into_any_element())
    }

    fn header(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let core = self.model.read(cx).core();
        let (title, topic) = core
            .selected_conversation()
            .and_then(|id| core.conversation(id))
            .map(|conversation| {
                (
                    format!("# {}", conversation.name),
                    conversation.topic.clone().unwrap_or_default(),
                )
            })
            .unwrap_or_default();
        div()
            .h(px(52.))
            .flex_none()
            .px_5()
            .flex()
            .items_center()
            .gap_3()
            .border_b_1()
            .border_color(theme.colors.border)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(theme.type_scale.title)
                    .text_color(theme.colors.text)
                    .child(title),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(theme.type_scale.small)
                    .text_color(theme.colors.text_muted)
                    .child(topic),
            )
            .into_any_element()
    }
}

impl Render for ShellView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let compact = f32::from(window.viewport_size().width) < COMPACT_BELOW;
        let workspace_name = self
            .model
            .read(cx)
            .core()
            .primary_workspace()
            .map(|info| info.workspace.name.clone())
            .unwrap_or_default();

        let mut main = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme.colors.surface)
            .child(self.header(&theme, cx));
        if let Some(banner) = self.banner(&theme, cx) {
            main = main.child(banner);
        }
        main = main
            .child(div().flex_1().min_h_0().child(self.timeline.clone()))
            .child(self.composer.clone());

        let sidebar_width = if compact {
            COMPACT_SIDEBAR_WIDTH
        } else {
            SIDEBAR_WIDTH
        };
        div()
            .size_full()
            .key_context(keyboard::SHELL_CONTEXT)
            .on_action(cx.listener(|_, _: &FocusNextControl, window, cx| window.focus_next(cx)))
            .on_action(cx.listener(|_, _: &FocusPrevControl, window, cx| window.focus_prev(cx)))
            .flex()
            .bg(theme.colors.surface)
            .when(!compact, |layout| {
                layout.child(self.rail(&theme, &workspace_name))
            })
            .child(self.sidebar(&theme, sidebar_width, cx))
            .child(main)
    }
}
