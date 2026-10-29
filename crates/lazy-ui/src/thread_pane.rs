//! The thread pane: a message's replies beside the conversation.
//!
//! The pane is a column at the right of the window: a header (what thread this is, whether the
//! person follows it, and a way out), the root message and its replies in a timeline of their own,
//! and a composer that sends replies. A grab strip on its left edge resizes it, with the pointer
//! or, when focused, with Left and Right. Where the window is too narrow for both the
//! conversation and the pane, the shell draws the pane alone and the header shows Back instead of
//! Close (see [`crate::thread_layout`]).
//!
//! # Keyboard
//!
//! Tab stops inside the pane, in order: the resize handle, Back (narrow windows only), the follow
//! button, Close (side by side), the replies (one stop, a roving cursor like the conversation's),
//! the reply box, the "Also send to" checkbox, the emoji button, Send. Escape closes the pane
//! unless a menu, an editor, or a suggestion list has it first.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, FontWeight, IntoElement, KeyDownEvent, Render,
    Role, Subscription, Window, div, prelude::*, px,
};
use lazy_client_core::Input;

use crate::app_model::AppModel;
use crate::composer::Composer;
use crate::form::{self, ButtonKind};
use crate::theme::Theme;
use crate::thread_layout::{self, HANDLE_WIDTH};
use crate::timeline::TimelineView;

/// What the pane asks of the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadPaneEvent {
    /// Close the pane (or go back to the conversation, in a narrow window).
    Close,
}

/// How the shell lays the pane out this frame; shared so the pane can read it without the shell
/// updating it while the shell renders.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneLayout {
    /// The drawn width of the pane's body, beside the handle.
    pub width: f32,
    /// The pane replaces the conversation: Back instead of Close, and no handle.
    pub replaces_conversation: bool,
}

impl Default for PaneLayout {
    fn default() -> Self {
        Self {
            width: thread_layout::DEFAULT_PANE_WIDTH,
            replaces_conversation: false,
        }
    }
}

/// The payload of a drag that resizes the pane. The shell listens for it across the window.
#[derive(Debug, Clone, Copy)]
pub struct ThreadResizeDrag;

/// What is dragged: nothing visible; the pane itself follows the pointer.
struct NoDragImage;

impl Render for NoDragImage {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_0()
    }
}

pub struct ThreadPane {
    model: Entity<AppModel>,
    timeline: Entity<TimelineView>,
    composer: Entity<Composer>,
    layout: Rc<Cell<PaneLayout>>,
    handle_focus: FocusHandle,
    follow_focus: FocusHandle,
    close_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ThreadPaneEvent> for ThreadPane {}

impl ThreadPane {
    pub fn new(
        model: Entity<AppModel>,
        layout: Rc<Cell<PaneLayout>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let timeline = cx.new(|cx| TimelineView::for_thread(model.clone(), window, cx));
        let composer = cx.new(|cx| Composer::for_thread(model.clone(), window, cx));
        let observed = cx.observe(&model, |_, _, cx| cx.notify());
        Self {
            model,
            timeline,
            composer,
            layout,
            handle_focus: cx.focus_handle().tab_stop(true),
            follow_focus: cx.focus_handle().tab_stop(true),
            close_focus: cx.focus_handle().tab_stop(true),
            _subscriptions: vec![observed],
        }
    }

    /// The pane's replies, for the shell to route their requests and for tests.
    pub fn timeline(&self) -> &Entity<TimelineView> {
        &self.timeline
    }

    /// The pane's reply composer.
    pub fn composer(&self) -> &Entity<Composer> {
        &self.composer
    }

    /// The resize handle's focus, for tests of the tab order.
    pub fn handle_focus(&self) -> FocusHandle {
        self.handle_focus.clone()
    }

    /// The follow button's focus, for tests of the tab order.
    pub fn follow_focus(&self) -> FocusHandle {
        self.follow_focus.clone()
    }

    /// The close (or back) button's focus, for tests of the tab order.
    pub fn close_focus(&self) -> FocusHandle {
        self.close_focus.clone()
    }

    fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if event.is_held {
            return;
        }
        let current = self.layout.get().width;
        if let Some(wish) = thread_layout::wish_from_key(
            current,
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.shift,
        ) {
            self.model
                .update(cx, |model, cx| model.set_thread_pane_width(wish, cx));
            cx.stop_propagation();
        }
    }

    fn header(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = theme.colors;
        let layout = self.layout.get();
        let (root, conversation, following, title) = {
            let core = self.model.read(cx).core();
            match core.open_thread() {
                Some(open) => {
                    let following = core
                        .thread(open.root)
                        .and_then(|view| view.state)
                        .is_some_and(|state| state.following);
                    let name = core.conversation_title(open.conversation);
                    let is_channel = core
                        .conversation(open.conversation)
                        .is_some_and(|c| c.kind == lazy_domain::ConversationKind::Channel);
                    let title = if is_channel {
                        format!("# {name}")
                    } else {
                        name
                    };
                    (Some(open.root), Some(open.conversation), following, title)
                }
                None => (None, None, false, String::new()),
            }
        };
        let _ = conversation;
        let this = cx.entity().downgrade();
        let close_to = this.clone();
        let follow_model = self.model.clone();
        let close_label = if layout.replaces_conversation {
            "← Back"
        } else {
            "Close"
        };
        div()
            .debug_selector(|| "thread-header".into())
            .h(px(52.))
            .flex_none()
            .px_4()
            .flex()
            .items_center()
            .gap_3()
            .border_b_1()
            .border_color(colors.border)
            .when(layout.replaces_conversation, |header| {
                header.child(form::button(
                    theme,
                    "thread-back",
                    close_label,
                    ButtonKind::Secondary,
                    true,
                    &self.close_focus,
                    {
                        let close_to = close_to.clone();
                        move |_, cx| {
                            close_to
                                .update(cx, |_, cx| cx.emit(ThreadPaneEvent::Close))
                                .ok();
                        }
                    },
                ))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(theme.type_scale.title)
                            .text_color(colors.text)
                            .child("Thread"),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(theme.type_scale.small)
                            .text_color(colors.text_muted)
                            .child(title),
                    ),
            )
            .child(form::button(
                theme,
                "thread-follow",
                if following {
                    "Unfollow thread"
                } else {
                    "Follow thread"
                },
                ButtonKind::Secondary,
                root.is_some(),
                &self.follow_focus,
                move |_, cx| {
                    if let Some(root) = root {
                        follow_model.update(cx, |model, cx| {
                            model.dispatch(
                                Input::SetThreadFollow {
                                    root,
                                    following: !following,
                                },
                                cx,
                            )
                        });
                    }
                },
            ))
            .when(!layout.replaces_conversation, |header| {
                header.child(form::button(
                    theme,
                    "thread-close",
                    close_label,
                    ButtonKind::Secondary,
                    true,
                    &self.close_focus,
                    move |_, cx| {
                        close_to
                            .update(cx, |_, cx| cx.emit(ThreadPaneEvent::Close))
                            .ok();
                    },
                ))
            })
    }
}

impl Render for ThreadPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        let layout = self.layout.get();
        let width = layout.width;
        let header = self.header(&theme, cx);
        let handle_focused = self.handle_focus.is_focused(window);
        let handle = (!layout.replaces_conversation).then(|| {
            // A strip wide enough to grab, with a thin line in it that thickens when the pointer
            // is over it, while it is dragged, and while it has the keyboard.
            div()
                .id("thread-resize-handle")
                .debug_selector(|| "thread-resize-handle".into())
                .track_focus(&self.handle_focus)
                .role(Role::Splitter)
                .aria_label("Resize thread pane")
                .aria_numeric_value(f64::from(width))
                .aria_min_numeric_value(f64::from(thread_layout::MIN_PANE_WIDTH))
                .aria_max_numeric_value(f64::from(thread_layout::MAX_PANE_WIDTH))
                .group("thread-handle")
                .w(px(HANDLE_WIDTH))
                .h_full()
                .flex_none()
                .flex()
                .justify_center()
                .cursor_col_resize()
                .on_drag(ThreadResizeDrag, |_, _, _, cx| cx.new(|_| NoDragImage))
                .on_key_down(
                    cx.listener(|this, event: &KeyDownEvent, _, cx| this.handle_key(event, cx)),
                )
                .child(
                    div()
                        .h_full()
                        .w(px(if handle_focused { 3. } else { 1. }))
                        .bg(if handle_focused {
                            colors.focus_ring
                        } else {
                            colors.border
                        })
                        .group_hover("thread-handle", move |style| {
                            style.w(px(3.)).bg(colors.border_strong)
                        }),
                )
        });

        div()
            .id("thread-pane")
            .debug_selector(|| "thread-pane".into())
            .role(Role::Complementary)
            .aria_label("Thread")
            .h_full()
            .flex_none()
            .when(layout.replaces_conversation, |pane| pane.flex_1().min_w_0())
            .when(!layout.replaces_conversation, |pane| {
                pane.w(px(width + HANDLE_WIDTH))
            })
            .flex()
            // Files dropped on the pane are attached to the reply.
            .on_drop::<gpui::ExternalPaths>(cx.listener(
                |this, paths: &gpui::ExternalPaths, _window, cx| {
                    this.composer.update(cx, |composer, cx| {
                        composer.attach_paths(paths.paths().to_vec(), cx)
                    });
                },
            ))
            // Escape closes the pane unless something inside it (a menu, an editor, a suggestion
            // list, an input method) took the key first.
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape"
                    && !event.keystroke.modifiers.modified()
                    && !event.is_held
                {
                    cx.emit(ThreadPaneEvent::Close);
                    cx.stop_propagation();
                }
            }))
            .children(handle)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(colors.surface)
                    .child(header)
                    .child(div().flex_1().min_h_0().child(self.timeline.clone()))
                    .child(self.composer.clone()),
            )
    }
}
