//! The message composer: a growing text input bound to the selected conversation's draft.
//!
//! The core owns drafts. The composer reports edits with `SetDraft`, sends with `SendMessage`,
//! and follows the core when the draft changes underneath it (a send clears it, a conversation
//! switch loads another). Text is only replaced when it differs, so an IME composition in
//! progress is never overwritten.

use gpui::{
    Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement, Render, Subscription, Window,
    div, prelude::*, px,
};
use lazy_client_core::{Change, Input};
use lazy_domain::ConversationId;
use lazy_domain::document::MAX_TEXT_BYTES;

use crate::app_model::{AppEvent, AppModel};
use crate::keyboard;
use crate::text_input::{TextInput, TextInputEvent, TextInputOptions, TextInputStyle};
use crate::theme::Theme;

/// A text-input style that takes its colors and metrics from the theme.
pub fn input_style(theme: &Theme) -> TextInputStyle {
    TextInputStyle {
        text_color: theme.colors.text,
        placeholder_color: theme.colors.text_faint,
        selection_color: theme.colors.selection,
        caret_color: theme.colors.caret,
        font_size: theme.type_scale.body,
        line_height: theme.type_scale.line_height_body,
        padding_x: px(0.),
        padding_y: px(0.),
        caret_width: px(1.5),
    }
}

pub struct Composer {
    model: Entity<AppModel>,
    input: Entity<TextInput>,
    conversation: Option<ConversationId>,
    send_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Composer {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let theme = *Theme::get(cx);
        let input = cx.new(|cx| {
            TextInput::new(
                TextInputOptions::new("Message")
                    .accessible_label("Message")
                    .max_lines(8)
                    .max_bytes(MAX_TEXT_BYTES)
                    .style(input_style(&theme)),
                cx,
            )
        });

        let input_events =
            cx.subscribe(
                &input,
                |this, _input, event: &TextInputEvent, cx| match event {
                    TextInputEvent::Changed => this.report_draft(cx),
                    TextInputEvent::Submit => this.submit(cx),
                    TextInputEvent::Tab { .. } => {}
                },
            );
        let model_events = cx.subscribe_in(
            &model,
            window,
            |this, _model, event: &AppEvent, window, cx| {
                let AppEvent::Changed(changes) = event;
                this.follow_core(changes, window, cx);
            },
        );

        let mut composer = Self {
            model,
            input,
            conversation: None,
            send_focus: cx.focus_handle().tab_stop(true),
            _subscriptions: vec![input_events, model_events],
        };
        composer.switch_conversation(window, cx);
        composer
    }

    fn selected(&self, cx: &Context<Self>) -> Option<ConversationId> {
        self.model.read(cx).core().selected_conversation()
    }

    /// Loads the selected conversation's draft into the input and focuses it.
    fn switch_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.conversation = self.selected(cx);
        let draft = self
            .conversation
            .map(|id| self.model.read(cx).core().draft(id).to_owned())
            .unwrap_or_default();
        let enabled = self.conversation.is_some();
        self.input.update(cx, |input, cx| {
            input.set_text(&draft, cx);
            input.set_enabled(enabled, cx);
        });
        if enabled {
            let handle = self.input.focus_handle(cx);
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    fn follow_core(&mut self, changes: &[Change], window: &mut Window, cx: &mut Context<Self>) {
        if self.selected(cx) != self.conversation {
            self.switch_conversation(window, cx);
            return;
        }
        for change in changes {
            if let Change::DraftChanged { conversation } = *change
                && Some(conversation) == self.conversation
            {
                let draft = self.model.read(cx).core().draft(conversation).to_owned();
                self.input.update(cx, |input, cx| {
                    if input.text() != draft {
                        input.set_text(&draft, cx);
                    }
                });
                cx.notify();
            }
        }
    }

    fn report_draft(&mut self, cx: &mut Context<Self>) {
        let Some(conversation) = self.conversation else {
            return;
        };
        let text = self.input.read(cx).text().to_owned();
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::SetDraft { conversation, text }, cx);
        });
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let Some(conversation) = self.conversation else {
            return;
        };
        let text = self.input.read(cx).text().to_owned();
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::SendMessage { conversation, text }, cx);
        });
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let focused = self.input.read(cx).is_focused(window);
        let has_text = !self.input.read(cx).text().trim().is_empty();
        let can_send = has_text && self.conversation.is_some();
        let channel = self
            .conversation
            .and_then(|id| self.model.read(cx).core().conversation(id))
            .map(|conversation| conversation.name.to_string());

        div()
            .flex_none()
            .px_5()
            .pb_4()
            .pt_2()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded(theme.radii.medium)
                    .bg(theme.colors.surface_raised)
                    .border_1()
                    .border_color(if focused { theme.colors.focus_ring } else { theme.colors.border_strong })
                    .child(div().w_full().child(self.input.clone()))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(theme.type_scale.small)
                                    .text_color(theme.colors.text_faint)
                                    .child(match channel {
                                        Some(name) => format!("Message #{name} · Enter to send, Shift+Enter for a new line"),
                                        None => "Select a conversation to write".to_owned(),
                                    }),
                            )
                            .child(
                                div()
                                    .id("send-message")
                                    .debug_selector(|| "send-message".into())
                                    .track_focus(&self.send_focus)
                                    .border_1()
                                    .border_color(gpui::transparent_black())
                                    .focus_visible(|style| style.border_color(theme.colors.focus_ring))
                                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                                        if keyboard::is_activation(event) {
                                            this.submit(cx);
                                            cx.stop_propagation();
                                        }
                                    }))
                                    .flex_none()
                                    .ml_3()
                                    .px_3()
                                    .py_1()
                                    .rounded(theme.radii.small)
                                    .text_size(theme.type_scale.small)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .when(can_send, |button| {
                                        button
                                            .cursor_pointer()
                                            .bg(theme.colors.accent)
                                            .text_color(theme.colors.on_accent)
                                            .hover(|style| style.bg(theme.colors.accent_hover))
                                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx)))
                                    })
                                    .when(!can_send, |button| {
                                        button
                                            .bg(theme.colors.surface_hover)
                                            .text_color(theme.colors.text_faint)
                                    })
                                    .child("Send"),
                            ),
                    ),
            )
    }
}
