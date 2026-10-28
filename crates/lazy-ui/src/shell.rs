//! The signed-in layout: workspace rail, conversation sidebar, and the conversation pane, with
//! the dialog layer above them.

use gpui::{
    AnyElement, AnyView, Context, Entity, EventEmitter, FocusHandle, FontWeight, IntoElement,
    ParentElement, Render, Styled, Subscription, Window, div, prelude::*, px,
};
use lazy_client_core::{ActionKind, ActionState, ConnectionState, Input};
use lazy_domain::{ChannelVisibility, ConversationId, ConversationKind};

use crate::app_model::AppModel;
use crate::composer::Composer;
use crate::dialogs::{
    BrowseChannelsDialog, CreateChannelDialog, DetailsDialog, DetailsRequest, PeopleDialog,
    PeopleMode,
};
use crate::form::{self, ButtonKind};
use crate::keyboard::{self, FocusNextControl, FocusPrevControl};
use crate::modal::DialogEvent;
use crate::sidebar::{SidebarEvent, SidebarView};
use crate::theme::Theme;
use crate::timeline::TimelineView;

const RAIL_WIDTH: f32 = 64.0;
const SIDEBAR_WIDTH: f32 = 260.0;
/// Below this window width the sidebar narrows and the workspace rail is hidden.
const COMPACT_BELOW: f32 = 760.0;
const COMPACT_SIDEBAR_WIDTH: f32 = 208.0;

/// The dialog currently open, with the subscriptions that close it.
struct Overlay {
    view: AnyView,
    _subscriptions: Vec<Subscription>,
}

/// A dialog the shell can open, for commands that are not a click on a visible control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellDialog {
    CreateChannel,
    BrowseChannels,
    NewMessage,
    Details(ConversationId),
    /// Add people to a channel.
    AddPeople(ConversationId),
}

pub struct ShellView {
    model: Entity<AppModel>,
    sidebar: Entity<SidebarView>,
    timeline: Entity<TimelineView>,
    composer: Entity<Composer>,
    overlay: Option<Overlay>,
    /// The conversation selected when the open dialog appeared, to notice when the dialog's work
    /// moved the person somewhere new (and then put them in that conversation's composer).
    selected_at_open: Option<ConversationId>,
    star_focus: FocusHandle,
    details_focus: FocusHandle,
    empty_browse_focus: FocusHandle,
    empty_message_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl ShellView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| SidebarView::new(model.clone(), cx));
        let timeline = cx.new(|cx| TimelineView::new(model.clone(), cx));
        let composer = cx.new(|cx| Composer::new(model.clone(), window, cx));
        let mut subscriptions = Vec::new();
        // Re-render when the core changes so the header, banners, and empty states stay current.
        subscriptions.push(cx.observe(&model, |_, _, cx| cx.notify()));
        subscriptions.push(cx.subscribe_in(
            &sidebar,
            window,
            |this, _sidebar, event: &SidebarEvent, window, cx| match event {
                SidebarEvent::CreateChannel => this.open_create_channel(window, cx),
                SidebarEvent::BrowseChannels => this.open_browse(window, cx),
                SidebarEvent::NewMessage => this.open_new_message(window, cx),
            },
        ));
        // Unread messages are marked read only while the window is active.
        subscriptions.push(cx.observe_window_activation(window, |this, window, cx| {
            let active = window.is_window_active();
            this.model.update(cx, |model, cx| {
                model.dispatch(Input::WindowActive(active), cx)
            });
        }));
        let active = window.is_window_active();
        model.update(cx, |model, cx| {
            model.dispatch(Input::WindowActive(active), cx)
        });
        Self {
            model,
            sidebar,
            timeline,
            composer,
            overlay: None,
            selected_at_open: None,
            star_focus: cx.focus_handle().tab_stop(true),
            details_focus: cx.focus_handle().tab_stop(true),
            empty_browse_focus: cx.focus_handle().tab_stop(true),
            empty_message_focus: cx.focus_handle().tab_stop(true),
            _subscriptions: subscriptions,
        }
    }

    // ----- Dialogs -----

    /// Opens `dialog`, replacing any dialog that is already open.
    pub fn open_dialog(
        &mut self,
        dialog: ShellDialog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match dialog {
            ShellDialog::CreateChannel => self.open_create_channel(window, cx),
            ShellDialog::BrowseChannels => self.open_browse(window, cx),
            ShellDialog::NewMessage => self.open_new_message(window, cx),
            ShellDialog::Details(id) => self.open_details(id, window, cx),
            ShellDialog::AddPeople(id) => {
                self.open_people(PeopleMode::AddToChannel(id), window, cx)
            }
        }
    }

    /// Shows `dialog` above everything, and closes it when it asks to be dismissed.
    fn show<D>(
        &mut self,
        dialog: Entity<D>,
        mut subscriptions: Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        D: Render + EventEmitter<DialogEvent> + 'static,
    {
        self.selected_at_open = self.model.read(cx).core().selected_conversation();
        subscriptions.push(cx.subscribe_in(
            &dialog,
            window,
            |this, _dialog, _event: &DialogEvent, window, cx| this.close_overlay(window, cx),
        ));
        self.overlay = Some(Overlay {
            view: dialog.into(),
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.overlay = None;
        // A dialog that created, opened, or joined a conversation selected it: write there.
        if self.model.read(cx).core().selected_conversation() != self.selected_at_open {
            self.composer
                .update(cx, |composer, cx| composer.focus_input(window, cx));
        }
        cx.notify();
    }

    fn open_create_channel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        let dialog = cx.new(|cx| CreateChannelDialog::new(model, window, cx));
        self.show(dialog, Vec::new(), window, cx);
    }

    fn open_browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        let dialog = cx.new(|cx| BrowseChannelsDialog::new(model, window, cx));
        self.show(dialog, Vec::new(), window, cx);
    }

    fn open_people(&mut self, mode: PeopleMode, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        let dialog = cx.new(|cx| PeopleDialog::new(model, mode, window, cx));
        self.show(dialog, Vec::new(), window, cx);
    }

    fn open_new_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_people(
            PeopleMode::NewConversation {
                prefill: Vec::new(),
                from: None,
            },
            window,
            cx,
        );
    }

    fn open_details(&mut self, id: ConversationId, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        let dialog = cx.new(|cx| DetailsDialog::new(model, id, window, cx));
        // The details dialog can ask for another dialog once it has closed itself.
        let request = cx.subscribe_in(
            &dialog,
            window,
            |this, _dialog, request: &DetailsRequest, window, cx| match request {
                DetailsRequest::AddPeopleToChannel(id) => {
                    this.open_people(PeopleMode::AddToChannel(*id), window, cx);
                }
                DetailsRequest::StartLargerGroup { from, prefill } => this.open_people(
                    PeopleMode::NewConversation {
                        prefill: prefill.clone(),
                        from: Some(*from),
                    },
                    window,
                    cx,
                ),
            },
        );
        self.show(dialog, vec![request], window, cx);
    }

    // ----- Pieces -----

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
        let favorite_error = core.selected_conversation().and_then(|id| {
            match core.action(ActionKind::SetFavorite(id)) {
                ActionState::Failed(message) => Some((id, message.clone())),
                _ => None,
            }
        });
        if connection.is_none() && notice.is_none() && favorite_error.is_none() {
            return None;
        }
        let colors = theme.colors;
        let mut stack = div().flex().flex_col();
        if let Some((text, is_warning)) = connection {
            stack = stack.child(
                div()
                    .px_4()
                    .py_1()
                    .text_size(theme.type_scale.small)
                    .bg(if is_warning {
                        colors.warning_surface
                    } else {
                        colors.surface_hover
                    })
                    .text_color(if is_warning {
                        colors.warning
                    } else {
                        colors.text_muted
                    })
                    .child(text),
            );
        }
        let dismiss = |id: &'static str| {
            div()
                .id(id)
                .cursor_pointer()
                .px_2()
                .rounded(theme.radii.small)
                .hover(move |style| style.bg(colors.surface_hover))
                .child("Dismiss")
        };
        if let Some(text) = notice {
            stack = stack.child(
                div()
                    .debug_selector(|| "notice-banner".into())
                    .px_4()
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_size(theme.type_scale.small)
                    .bg(colors.danger_surface)
                    .text_color(colors.danger)
                    .child(div().flex_1().child(text))
                    .child(
                        dismiss("dismiss-notice").on_click(cx.listener(|this, _, _, cx| {
                            this.model
                                .update(cx, |model, cx| model.dispatch(Input::DismissNotice, cx));
                        })),
                    ),
            );
        }
        if let Some((id, message)) = favorite_error {
            stack = stack.child(
                div()
                    .px_4()
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_size(theme.type_scale.small)
                    .bg(colors.danger_surface)
                    .text_color(colors.danger)
                    .child(
                        div()
                            .flex_1()
                            .child(format!("Couldn't change favorite: {message}")),
                    )
                    .child(dismiss("dismiss-favorite-error").on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.model.update(cx, |model, cx| {
                                model.dispatch(
                                    Input::DismissAction(ActionKind::SetFavorite(id)),
                                    cx,
                                );
                            });
                        },
                    ))),
            );
        }
        Some(stack.into_any_element())
    }

    fn header(&self, theme: &Theme, id: ConversationId, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let core = self.model.read(cx).core();
        let title = core.conversation_title(id);
        let conversation = core.conversation(id).cloned();
        let favorite = core.user_state(id).is_some_and(|state| state.favorite);
        let favoriting = core.action(ActionKind::SetFavorite(id)) == &ActionState::InFlight;
        let (prefix, topic, members, archived) = match &conversation {
            Some(c) if c.kind == ConversationKind::Channel => (
                if c.visibility == ChannelVisibility::Private {
                    "🔒 "
                } else {
                    "# "
                },
                c.topic.clone().unwrap_or_default(),
                Some(format!(
                    "{} member{}",
                    c.member_count,
                    if c.member_count == 1 { "" } else { "s" }
                )),
                c.archived,
            ),
            Some(c) => (
                "",
                String::new(),
                Some(format!("{} people", c.participants.len())),
                false,
            ),
            None => ("", String::new(), None, false),
        };
        let this = cx.entity().downgrade();
        let star_to = this.clone();
        let details_to = this.clone();
        div()
            .h(px(52.))
            .flex_none()
            .px_5()
            .flex()
            .items_center()
            .gap_3()
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .flex_none()
                    .max_w(px(260.))
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(theme.type_scale.title)
                    .text_color(colors.text)
                    .child(format!("{prefix}{title}")),
            )
            .children(archived.then(|| {
                div()
                    .flex_none()
                    .px_2()
                    .rounded(theme.radii.small)
                    .bg(colors.warning_surface)
                    .text_color(colors.warning)
                    .text_size(theme.type_scale.small)
                    .child("Archived")
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_muted)
                    .child(topic),
            )
            .children(members.map(|text| {
                div()
                    .flex_none()
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_faint)
                    .child(text)
            }))
            .child(form::button(
                theme,
                "favorite-toggle",
                if favorite {
                    "★ Favorited"
                } else {
                    "☆ Favorite"
                },
                ButtonKind::Secondary,
                !favoriting,
                &self.star_focus,
                move |_, cx| {
                    star_to
                        .update(cx, |shell, cx| {
                            shell.model.update(cx, |model, cx| {
                                model.dispatch(
                                    Input::SetFavorite {
                                        id,
                                        favorite: !favorite,
                                    },
                                    cx,
                                );
                            });
                        })
                        .ok();
                },
            ))
            .child(form::button(
                theme,
                "details-button",
                "Details",
                ButtonKind::Secondary,
                true,
                &self.details_focus,
                move |window, cx| {
                    details_to
                        .update(cx, |shell, cx| shell.open_details(id, window, cx))
                        .ok();
                },
            ))
            .into_any_element()
    }

    /// Shown instead of a conversation when none is selected.
    fn empty_state(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let browse_to = this.clone();
        let message_to = this;
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .px_8()
            .child(
                div()
                    .text_size(theme.type_scale.heading)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.text)
                    .child("You're not in any conversations yet"),
            )
            .child(
                div()
                    .text_size(theme.type_scale.body)
                    .text_color(colors.text_muted)
                    .child("Join a channel to see what your team is talking about, or message someone directly."),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(form::button(theme, "empty-browse", "Browse channels", ButtonKind::Primary, true, &self.empty_browse_focus, move |window, cx| {
                        browse_to.update(cx, |shell, cx| shell.open_browse(window, cx)).ok();
                    }))
                    .child(form::button(theme, "empty-message", "New message", ButtonKind::Secondary, true, &self.empty_message_focus, move |window, cx| {
                        message_to.update(cx, |shell, cx| shell.open_new_message(window, cx)).ok();
                    })),
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
        let selected = self.model.read(cx).core().selected_conversation();

        let mut main = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme.colors.surface);
        match selected {
            Some(id) => {
                main = main.child(self.header(&theme, id, cx));
                if let Some(banner) = self.banner(&theme, cx) {
                    main = main.child(banner);
                }
                main = main
                    .child(div().flex_1().min_h_0().child(self.timeline.clone()))
                    .child(self.composer.clone());
            }
            None => {
                if let Some(banner) = self.banner(&theme, cx) {
                    main = main.child(banner);
                }
                main = main.child(div().flex_1().min_h_0().child(self.empty_state(&theme, cx)));
            }
        }

        let sidebar_width = if compact {
            COMPACT_SIDEBAR_WIDTH
        } else {
            SIDEBAR_WIDTH
        };
        div()
            .size_full()
            .relative()
            .key_context(keyboard::SHELL_CONTEXT)
            .on_action(cx.listener(|_, _: &FocusNextControl, window, cx| window.focus_next(cx)))
            .on_action(cx.listener(|_, _: &FocusPrevControl, window, cx| window.focus_prev(cx)))
            .flex()
            .bg(theme.colors.surface)
            .when(!compact, |layout| {
                layout.child(self.rail(&theme, &workspace_name))
            })
            .child(
                div()
                    .w(px(sidebar_width))
                    .flex_none()
                    .h_full()
                    .bg(theme.colors.sidebar)
                    .border_r_1()
                    .border_color(theme.colors.border)
                    .child(self.sidebar.clone()),
            )
            .child(main)
            .children(self.overlay.as_ref().map(|overlay| overlay.view.clone()))
    }
}
