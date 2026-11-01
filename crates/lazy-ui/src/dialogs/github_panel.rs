//! GitHub, inside the Integrations dialog (admins and owners, D32): connecting the workspace's
//! GitHub App, installing it, which channels follow which repositories, recent deliveries with
//! Retry, and disconnecting.
//!
//! # Views
//!
//! - **Overview**: where things stand, and the next step. Not available (no credentials key on the
//!   server), not connected (organization and Connect GitHub), connected but not installed
//!   (Install on GitHub), or installed: subscriptions with Remove and Add a repository, recent
//!   deliveries with Retry on failed ones, and Disconnect….
//! - **Adding**: the channel, the repository (`owner/name` or a GitHub address), and which kinds of
//!   activity, then Follow.
//! - **Confirm disconnect**.
//!
//! Connecting continues in the browser: the one-time address the server answers with is opened
//! at once and forgotten (it is never shown). Coming back, Refresh (or reopening the dialog)
//! shows what GitHub has told the server.
//!
//! # Keyboard
//!
//! One tab stop per control, in the order drawn; Tab and Shift+Tab in the text fields move on
//! through the dialog. Enter in the organization connects; Enter in the repository follows.
//! Escape steps back one view; from the overview the dialog returns to its list.

use std::collections::HashMap;

use gpui::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    ParentElement, Render, Styled, Subscription, Window, div, prelude::*, px,
};
use jiff::Zoned;
use jiff::tz::TimeZone;
use lazy_client_core::{ActionKind, ActionState, Input};
use lazy_domain::github::GithubEvent;
use lazy_domain::{ConversationId, ConversationKind, GithubSubscriptionId, WorkspaceId};
use lazy_protocol::{GithubDelivery, GithubDeliveryState, GithubStatusResponse};

use super::integration_ui as ui;
use super::single_line_field;
use crate::app_model::{AppEvent, AppModel};
use crate::form::{self, ButtonKind};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::Theme;
use crate::time_fmt::{ago_label, to_zoned};

/// What the panel asks of the dialog around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubPanelEvent {
    /// Tab or Shift+Tab was pressed in one of the panel's text fields.
    Tab { backwards: bool },
    /// Back to the dialog's list of integrations.
    Leave,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubView {
    Overview,
    Adding,
    ConfirmDisconnect,
}

pub struct GithubPanel {
    model: Entity<AppModel>,
    workspace: WorkspaceId,
    view: GithubView,
    organization: Entity<TextInput>,
    repository: Entity<TextInput>,
    channel: Option<ConversationId>,
    events: Vec<GithubEvent>,
    /// The browser was sent to GitHub: say what to do there, and offer Refresh.
    opened_browser: bool,
    /// A request whose end moves the view, and whether it has been seen starting.
    waiting: Option<(ActionKind, bool)>,
    zone: TimeZone,
    back_focus: FocusHandle,
    connect_focus: FocusHandle,
    refresh_focus: FocusHandle,
    open_focus: FocusHandle,
    install_focus: FocusHandle,
    add_focus: FocusHandle,
    disconnect_focus: FocusHandle,
    cancel_focus: FocusHandle,
    confirm_focus: FocusHandle,
    remove_focus: HashMap<GithubSubscriptionId, FocusHandle>,
    retry_focus: HashMap<i64, FocusHandle>,
    channel_focus: HashMap<ConversationId, FocusHandle>,
    event_focus: HashMap<GithubEvent, FocusHandle>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<GithubPanelEvent> for GithubPanel {}

impl GithubPanel {
    pub fn new(
        model: Entity<AppModel>,
        workspace: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let organization = single_line_field("e.g. last-refuge", "GitHub organization", 39, cx);
        let repository =
            single_line_field("owner/name, e.g. last-refuge/lazy", "Repository", 200, cx);
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &organization,
            window,
            |this, _input, event: &TextInputEvent, _window, cx| match event {
                TextInputEvent::Submit => this.connect(cx),
                TextInputEvent::Tab { backwards } => cx.emit(GithubPanelEvent::Tab {
                    backwards: *backwards,
                }),
                TextInputEvent::Changed => cx.notify(),
                _ => {}
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &repository,
            window,
            |this, _input, event: &TextInputEvent, _window, cx| match event {
                TextInputEvent::Submit => this.follow(cx),
                TextInputEvent::Tab { backwards } => cx.emit(GithubPanelEvent::Tab {
                    backwards: *backwards,
                }),
                TextInputEvent::Changed => cx.notify(),
                _ => {}
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &model,
            window,
            |this, _model, _event: &AppEvent, window, cx| {
                this.follow_core(window, cx);
                cx.notify();
            },
        ));
        model.update(cx, |model, cx| {
            model.dispatch(Input::DismissGithubSetup, cx);
            model.dispatch(Input::LoadGithub(workspace), cx);
        });
        Self {
            model,
            workspace,
            view: GithubView::Overview,
            organization,
            repository,
            channel: None,
            events: GithubEvent::DEFAULT.to_vec(),
            opened_browser: false,
            waiting: None,
            zone: TimeZone::system(),
            back_focus: cx.focus_handle().tab_stop(true),
            connect_focus: cx.focus_handle().tab_stop(true),
            refresh_focus: cx.focus_handle().tab_stop(true),
            open_focus: cx.focus_handle().tab_stop(true),
            install_focus: cx.focus_handle().tab_stop(true),
            add_focus: cx.focus_handle().tab_stop(true),
            disconnect_focus: cx.focus_handle().tab_stop(true),
            cancel_focus: cx.focus_handle().tab_stop(true),
            confirm_focus: cx.focus_handle().tab_stop(true),
            remove_focus: HashMap::new(),
            retry_focus: HashMap::new(),
            channel_focus: HashMap::new(),
            event_focus: HashMap::new(),
            _subscriptions: subscriptions,
        }
    }

    // ----- For the dialog and tests -----

    pub fn view(&self) -> GithubView {
        self.view
    }

    /// Where the keyboard starts when the panel opens.
    pub fn first_focus(&self) -> FocusHandle {
        self.back_focus.clone()
    }

    pub fn organization_input(&self) -> &Entity<TextInput> {
        &self.organization
    }

    pub fn repository_input(&self) -> &Entity<TextInput> {
        &self.repository
    }

    pub fn composing(&self, cx: &gpui::App) -> bool {
        self.organization.read(cx).is_composing() || self.repository.read(cx).is_composing()
    }

    /// The action kinds the panel may have left failed, to clear when the dialog closes.
    pub fn kinds(&self) -> Vec<ActionKind> {
        let mut kinds = vec![
            ActionKind::LoadGithub(self.workspace),
            ActionKind::StartGithubSetup(self.workspace),
            ActionKind::DisconnectGithub(self.workspace),
        ];
        kinds.extend(self.channel.map(ActionKind::GithubSubscribe));
        kinds.extend(
            self.remove_focus
                .keys()
                .map(|id| ActionKind::GithubUnsubscribe(*id)),
        );
        kinds.extend(
            self.retry_focus
                .keys()
                .map(|id| ActionKind::RetryGithubDelivery(*id)),
        );
        kinds
    }

    /// Escape: back one view. Returns whether it did anything (from the overview it does not).
    pub fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.composing(cx) {
            return true;
        }
        match self.view {
            GithubView::Overview => false,
            GithubView::Adding | GithubView::ConfirmDisconnect => {
                self.show_overview(window, cx);
                true
            }
        }
    }

    // ----- Following the core -----

    fn status(&self, cx: &gpui::App) -> Option<GithubStatusResponse> {
        self.model.read(cx).core().github(self.workspace).cloned()
    }

    fn action(&self, kind: ActionKind, cx: &gpui::App) -> ActionState {
        self.model.read(cx).core().action(kind).clone()
    }

    fn follow_core(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The setup address: open it in the browser at once, and forget it.
        let url = self
            .model
            .read(cx)
            .core()
            .github_setup_url()
            .map(|url| url.as_str().to_owned());
        if let Some(url) = url {
            cx.open_url(&url);
            self.opened_browser = true;
            self.model.update(cx, |model, cx| {
                model.dispatch(Input::DismissGithubSetup, cx)
            });
        }
        if let Some((kind, started)) = self.waiting {
            match self.action(kind, cx) {
                ActionState::InFlight => self.waiting = Some((kind, true)),
                ActionState::Idle if started => {
                    self.waiting = None;
                    if matches!(
                        kind,
                        ActionKind::GithubSubscribe(_) | ActionKind::DisconnectGithub(_)
                    ) {
                        self.show_overview(window, cx);
                    }
                }
                ActionState::Failed(_) => self.waiting = None,
                ActionState::Idle => {}
            }
        }
        // Connected now: the browser step is done.
        if self.status(cx).is_some_and(|s| s.connection.is_some()) && self.opened_browser {
            let installed = self.status(cx).is_some_and(|s| !s.installations.is_empty());
            if installed {
                self.opened_browser = false;
            }
        }
    }

    fn dispatch(&self, input: Input, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
    }

    // ----- Moving and acting -----

    fn show_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.view = GithubView::Overview;
        window.focus(&self.back_focus, cx);
        cx.notify();
    }

    /// The channels a repository can be followed in: the person's open channels here, by name.
    fn channels(&self, cx: &gpui::App) -> Vec<(ConversationId, String)> {
        let mut channels: Vec<(ConversationId, String)> = self
            .model
            .read(cx)
            .core()
            .conversations()
            .iter()
            .filter(|c| {
                c.workspace_id == self.workspace
                    && c.kind == ConversationKind::Channel
                    && !c.archived
            })
            .filter_map(|c| Some((c.id, c.name.as_ref()?.as_str().to_owned())))
            .collect();
        channels.sort_by(|a, b| a.1.cmp(&b.1));
        channels
    }

    pub fn start_adding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let channels = self.channels(cx);
        let selected = self.model.read(cx).core().selected_conversation();
        self.channel = selected
            .filter(|id| channels.iter().any(|(c, _)| c == id))
            .or_else(|| channels.first().map(|(id, _)| *id));
        if let Some(channel) = self.channel {
            self.dispatch(
                Input::DismissAction(ActionKind::GithubSubscribe(channel)),
                cx,
            );
        }
        self.events = GithubEvent::DEFAULT.to_vec();
        self.repository
            .update(cx, |input, cx| input.set_text("", cx));
        self.view = GithubView::Adding;
        let handle = self.repository.focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        let kind = ActionKind::StartGithubSetup(self.workspace);
        if self.action(kind, cx) == ActionState::InFlight {
            return;
        }
        let organization = self.organization.read(cx).text().trim().to_owned();
        self.dispatch(
            Input::StartGithubSetup {
                workspace: self.workspace,
                organization,
            },
            cx,
        );
    }

    fn follow(&mut self, cx: &mut Context<Self>) {
        let Some(conversation) = self.channel else {
            return;
        };
        let kind = ActionKind::GithubSubscribe(conversation);
        if self.action(kind, cx) == ActionState::InFlight {
            return;
        }
        let repository = self.repository.read(cx).text().trim().to_owned();
        self.waiting = Some((kind, false));
        self.dispatch(
            Input::GithubSubscribe {
                conversation,
                repository,
                events: self.events.clone(),
            },
            cx,
        );
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.dispatch(Input::LoadGithub(self.workspace), cx);
    }

    fn handle_for<K: std::hash::Hash + Eq + Copy>(
        map: &mut HashMap<K, FocusHandle>,
        key: K,
        cx: &mut Context<Self>,
    ) -> FocusHandle {
        map.entry(key)
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone()
    }

    // ----- Drawing -----

    fn note(&self, theme: &Theme, selector: &'static str, text: impl Into<String>) -> AnyElement {
        div()
            .debug_selector(move || selector.into())
            .text_size(theme.type_scale.small)
            .text_color(theme.colors.text_muted)
            .child(text.into())
            .into_any_element()
    }

    fn error_line(&self, theme: &Theme, message: String) -> AnyElement {
        div()
            .debug_selector(|| "github-error".into())
            .text_size(theme.type_scale.small)
            .text_color(theme.colors.danger)
            .child(message)
            .into_any_element()
    }

    fn section_title(&self, theme: &Theme, text: &'static str) -> AnyElement {
        ui::section(theme, text, None)
    }

    fn header(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let leave_to = cx.entity().downgrade();
        let state = self.status(cx).map(|s| match &s.connection {
            None => ui::chip(theme, "Not connected".into(), ui::Tone::Quiet),
            Some(_) if s.installations.is_empty() => {
                ui::chip(theme, "Not installed".into(), ui::Tone::Attention)
            }
            Some(_) => ui::chip(theme, "Connected".into(), ui::Tone::Good),
        });
        ui::header(
            theme,
            "github-back".into(),
            "‹ All integrations",
            &self.back_focus,
            move |_, cx| {
                leave_to
                    .update(cx, |_, cx| cx.emit(GithubPanelEvent::Leave))
                    .ok();
            },
            Some(ui::service_mark(
                theme,
                ui::marks::GITHUB.0,
                ui::marks::GITHUB.1,
                36.,
            )),
            "GitHub".into(),
            state,
        )
    }

    fn overview(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let load = ActionKind::LoadGithub(self.workspace);
        let loading = self.action(load, cx) == ActionState::InFlight;
        let load_error = match self.action(load, cx) {
            ActionState::Failed(message) => Some(message),
            _ => None,
        };
        let status = self.status(cx);
        let mut body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.header(theme, cx));
        let refresh_to = this.clone();
        let refresh = form::button(
            theme,
            "github-refresh",
            if loading { "Refreshing…" } else { "Refresh" },
            ButtonKind::Secondary,
            !loading,
            &self.refresh_focus,
            move |_, cx| {
                refresh_to.update(cx, |panel, cx| panel.refresh(cx)).ok();
            },
        );
        let Some(status) = status else {
            body = body
                .children(
                    load_error.map(|m| {
                        self.error_line(theme, format!("GitHub's status didn't load. {m}"))
                    }),
                )
                .when(loading, |b| {
                    b.child(self.note(theme, "github-loading", "Loading…"))
                })
                .child(div().flex().child(refresh));
            return body.into_any_element();
        };
        if !status.available {
            return body
                .child(self.note(
                    theme,
                    "github-unavailable",
                    "This server can't hold GitHub's keys yet. Whoever runs it needs to set \
                     LAZY_CREDENTIALS_KEY (see DEPLOY.md, \"The credentials key\") and restart it.",
                ))
                .child(div().flex().child(refresh))
                .into_any_element();
        }
        let Some(connection) = status.connection.clone() else {
            // Not connected: the organization, and Connect.
            let setup = ActionKind::StartGithubSetup(self.workspace);
            let working = self.action(setup, cx) == ActionState::InFlight;
            let error = match self.action(setup, cx) {
                ActionState::Failed(message) => Some(message),
                _ => None,
            };
            let connect_to = this.clone();
            let ready = !working && !self.organization.read(cx).text().trim().is_empty();
            return body
                .child(self.note(
                    theme,
                    "github-intro",
                    "Lazy creates a GitHub App for this workspace, owned by your organization and \
                     usable only there. It posts pull requests, issues, commits and more into the \
                     channels you choose.",
                ))
                .child(form::labeled_field(
                    theme,
                    "GitHub organization",
                    &self.organization,
                    Some("The name in github.com/name. You need to be an owner of it.".into()),
                    error.map(Into::into),
                    window,
                    cx,
                ))
                .when(self.opened_browser, |b| {
                    b.child(self.note(
                        theme,
                        "github-continue",
                        "Finish in your browser: create the app on GitHub, then install it on the \
                         organization. Then come back and choose Refresh.",
                    ))
                })
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(form::button(
                            theme,
                            "github-connect",
                            if working {
                                "Opening GitHub…"
                            } else {
                                "Connect GitHub"
                            },
                            ButtonKind::Primary,
                            ready,
                            &self.connect_focus,
                            move |_, cx| {
                                connect_to.update(cx, |panel, cx| panel.connect(cx)).ok();
                            },
                        ))
                        .when(self.opened_browser, |row| row.child(refresh)),
                )
                .into_any_element();
        };

        // Connected.
        let open_url = connection.html_url.clone();
        let install_url = format!("{}/installations/new", connection.html_url);
        let installed = !status.installations.is_empty();
        let summary = if installed {
            status
                .installations
                .iter()
                .map(|i| {
                    format!(
                        "Installed on {} ({}){}",
                        i.account,
                        if i.all_repositories {
                            "all repositories"
                        } else {
                            "chosen repositories"
                        },
                        if i.suspended { ", suspended" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join(". ")
        } else {
            format!(
                "Not installed yet. Install it on {}, choosing all repositories (or the ones \
                 you want), then choose Refresh.",
                connection.owner
            )
        };
        body = body
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .debug_selector(|| "github-connected".into())
                                    .text_size(theme.type_scale.body)
                                    .text_color(colors.text)
                                    .child(format!(
                                        "The app {}, owned by {}",
                                        connection.app_slug, connection.owner
                                    )),
                            )
                            .child(
                                div()
                                    .debug_selector(|| "github-installation".into())
                                    .text_size(theme.type_scale.small)
                                    .text_color(if installed {
                                        colors.text_muted
                                    } else {
                                        colors.warning
                                    })
                                    .child(summary),
                            ),
                    )
                    .child(form::button(
                        theme,
                        "github-open",
                        "Open on GitHub",
                        ButtonKind::Secondary,
                        true,
                        &self.open_focus,
                        move |_, cx| cx.open_url(&open_url),
                    )),
            )
            .when(!installed, |b| {
                b.child(
                    div()
                        .flex()
                        .gap_2()
                        .child(form::button(
                            theme,
                            "github-install",
                            "Install on GitHub",
                            ButtonKind::Primary,
                            true,
                            &self.install_focus,
                            move |_, cx| cx.open_url(&install_url),
                        ))
                        .child(refresh),
                )
            });

        if installed {
            body = body
                .child(self.section_title(theme, "Channels following repositories"))
                .child(self.subscriptions(&status, theme, cx));
            let add_to = this.clone();
            body = body.child(div().flex().child(form::button(
                theme,
                "github-add",
                "Add a repository",
                ButtonKind::Primary,
                !self.channels(cx).is_empty(),
                &self.add_focus,
                move |window, cx| {
                    add_to
                        .update(cx, |panel, cx| panel.start_adding(window, cx))
                        .ok();
                },
            )));
            body = body
                .child(ui::divider(theme))
                .child(self.section_title(theme, "Recent deliveries"))
                .child(self.deliveries(&status.deliveries, theme, cx));
        }
        body = body.child(ui::divider(theme));
        let disconnect_to = this;
        body.child(div().flex().justify_end().child(form::button(
            theme,
            "github-disconnect",
            "Disconnect…",
            ButtonKind::Danger,
            true,
            &self.disconnect_focus,
            move |window, cx| {
                disconnect_to
                    .update(cx, |panel, cx| {
                        panel.dispatch(
                            Input::DismissAction(ActionKind::DisconnectGithub(panel.workspace)),
                            cx,
                        );
                        panel.view = GithubView::ConfirmDisconnect;
                        window.focus(&panel.cancel_focus, cx);
                        cx.notify();
                    })
                    .ok();
            },
        )))
        .into_any_element()
    }

    fn subscriptions(
        &mut self,
        status: &GithubStatusResponse,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        if status.subscriptions.is_empty() {
            return self.note(
                theme,
                "github-no-subscriptions",
                "No channel follows a repository yet. Add one to start.",
            );
        }
        let this = cx.entity().downgrade();
        let mut list = div()
            .id("github-subscriptions")
            .debug_selector(|| "github-subscriptions".into())
            .max_h(px(200.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1();
        for (index, sub) in status.subscriptions.iter().enumerate() {
            let id = sub.id;
            let focus = Self::handle_for(&mut self.remove_focus, id, cx);
            let state = self.action(ActionKind::GithubUnsubscribe(id), cx);
            let kinds = sub
                .events
                .iter()
                .map(|e| e.label())
                .collect::<Vec<_>>()
                .join(", ");
            let remove_to = this.clone();
            list = list.child(
                div()
                    .debug_selector(|| "github-subscription".into())
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .rounded(theme.radii.panel)
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.surface)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme.type_scale.body)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(colors.text)
                                    .child(format!(
                                        "{} → #{}",
                                        sub.repository,
                                        sub.channel_name.as_str()
                                    )),
                            )
                            .child(
                                div()
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.text_muted)
                                    .child(kinds),
                            )
                            .children(match &state {
                                ActionState::Failed(message) => {
                                    Some(self.error_line(theme, message.clone()))
                                }
                                _ => None,
                            }),
                    )
                    .child(form::button(
                        theme,
                        format!("github-remove-{index}"),
                        if state == ActionState::InFlight {
                            "Removing…"
                        } else {
                            "Remove"
                        },
                        ButtonKind::Secondary,
                        state != ActionState::InFlight,
                        &focus,
                        move |_, cx| {
                            remove_to
                                .update(cx, |panel, cx| {
                                    panel.dispatch(Input::GithubUnsubscribe(id), cx)
                                })
                                .ok();
                        },
                    )),
            );
        }
        list.into_any_element()
    }

    fn deliveries(
        &mut self,
        deliveries: &[GithubDelivery],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        if deliveries.is_empty() {
            return self.note(
                theme,
                "github-no-deliveries",
                "Nothing from GitHub yet. Activity in a followed repository shows here as it \
                 arrives.",
            );
        }
        let this = cx.entity().downgrade();
        let now = Zoned::now();
        let workspace = self.workspace;
        let mut rows = div()
            .id("github-deliveries")
            .debug_selector(|| "github-deliveries".into())
            .max_h(px(220.))
            .overflow_y_scroll()
            .flex()
            .flex_col();
        for (index, delivery) in deliveries.iter().enumerate() {
            let id = delivery.id;
            let (word, color) = match delivery.state {
                GithubDeliveryState::Posted => ("Posted", colors.success),
                GithubDeliveryState::Queued if delivery.attempts > 0 => {
                    ("Will retry", colors.warning)
                }
                GithubDeliveryState::Queued => ("Waiting", colors.text_muted),
                GithubDeliveryState::Running => ("Posting", colors.text_muted),
                GithubDeliveryState::Failed => ("Failed", colors.danger),
                GithubDeliveryState::Unknown => ("Not known", colors.warning),
                GithubDeliveryState::Other => ("Unknown state", colors.text_muted),
            };
            let what = match &delivery.action {
                Some(action) => format!("{} {action}", delivery.event.replace('_', " ")),
                None => delivery.event.replace('_', " "),
            };
            let when = ago_label(&to_zoned(delivery.received_at, &self.zone), &now);
            let mut facts = vec![what];
            facts.extend(delivery.repository.clone());
            facts.push(when);
            let retryable = matches!(
                delivery.state,
                GithubDeliveryState::Failed | GithubDeliveryState::Unknown
            );
            let retry_state = self.action(ActionKind::RetryGithubDelivery(id), cx);
            let focus = retryable.then(|| Self::handle_for(&mut self.retry_focus, id, cx));
            let retry_to = this.clone();
            rows = rows.child(
                div()
                    .debug_selector(|| "github-delivery".into())
                    .flex()
                    .items_center()
                    .gap_3()
                    .py_1p5()
                    .when(index + 1 < deliveries.len(), |row| row.border_b_1())
                    .border_color(colors.border)
                    .child(
                        div()
                            .w(px(110.))
                            .flex_none()
                            .text_size(theme.type_scale.small)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(color)
                            .child(word),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.text)
                                    .child(facts.join(" · ")),
                            )
                            .children(delivery.last_error.clone().filter(|_| retryable).map(
                                |error| {
                                    div()
                                        .text_size(theme.type_scale.small)
                                        .text_color(colors.text_muted)
                                        .child(error)
                                },
                            ))
                            .children(match retry_state {
                                ActionState::Failed(message) => {
                                    Some(self.error_line(theme, message))
                                }
                                _ => None,
                            }),
                    )
                    .children(focus.map(|focus| {
                        form::button(
                            theme,
                            format!("github-retry-{index}"),
                            "Retry",
                            ButtonKind::Secondary,
                            true,
                            &focus,
                            move |_, cx| {
                                retry_to
                                    .update(cx, |panel, cx| {
                                        panel.dispatch(
                                            Input::RetryGithubDelivery {
                                                workspace,
                                                delivery: id,
                                            },
                                            cx,
                                        )
                                    })
                                    .ok();
                            },
                        )
                    })),
            );
        }
        rows.into_any_element()
    }

    fn adding(&mut self, theme: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let channels = self.channels(cx);
        let state = self
            .channel
            .map(|c| self.action(ActionKind::GithubSubscribe(c), cx))
            .unwrap_or(ActionState::Idle);
        let working = state == ActionState::InFlight;
        let error = match &state {
            ActionState::Failed(message) => Some(message.clone()),
            _ => None,
        };
        let ready = self.channel.is_some()
            && !self.events.is_empty()
            && !working
            && !self.repository.read(cx).text().trim().is_empty();
        let mut choices = div()
            .id("github-channel-choices")
            .max_h(px(120.))
            .overflow_y_scroll()
            .flex()
            .flex_wrap()
            .gap_1();
        for (index, (id, name)) in channels.iter().enumerate() {
            let id = *id;
            let focus = Self::handle_for(&mut self.channel_focus, id, cx);
            let choose_to = this.clone();
            choices = choices.child(form::button(
                theme,
                format!("github-channel-{index}"),
                format!("#{name}"),
                if self.channel == Some(id) {
                    ButtonKind::Primary
                } else {
                    ButtonKind::Secondary
                },
                !working,
                &focus,
                move |_, cx| {
                    choose_to
                        .update(cx, |panel, cx| {
                            if let Some(old) = panel.channel {
                                panel.dispatch(
                                    Input::DismissAction(ActionKind::GithubSubscribe(old)),
                                    cx,
                                );
                            }
                            panel.channel = Some(id);
                            cx.notify();
                        })
                        .ok();
                },
            ));
        }
        let mut kinds = div().flex().flex_wrap().gap_1();
        for event in GithubEvent::ALL {
            let focus = Self::handle_for(&mut self.event_focus, event, cx);
            let on = self.events.contains(&event);
            let toggle_to = this.clone();
            kinds = kinds.child(form::button(
                theme,
                format!("github-event-{}", event.as_str()),
                if on {
                    format!("✓ {}", event.label())
                } else {
                    event.label().to_owned()
                },
                if on {
                    ButtonKind::Primary
                } else {
                    ButtonKind::Secondary
                },
                !working,
                &focus,
                move |_, cx| {
                    toggle_to
                        .update(cx, |panel, cx| {
                            if let Some(at) = panel.events.iter().position(|e| *e == event) {
                                panel.events.remove(at);
                            } else {
                                panel.events.push(event);
                                panel.events.sort_unstable();
                            }
                            cx.notify();
                        })
                        .ok();
                },
            ));
        }
        let cancel_to = this.clone();
        let follow_to = this;
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.header(theme, cx))
            .child(self.section_title(theme, "Channel"))
            .child(choices)
            .child(form::labeled_field(
                theme,
                "Repository",
                &self.repository,
                Some("owner/name, or the repository's GitHub address.".into()),
                error.map(Into::into),
                window,
                cx,
            ))
            .child(self.section_title(theme, "What to post"))
            .child(kinds)
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "github-cancel",
                        "Cancel",
                        ButtonKind::Secondary,
                        true,
                        &self.cancel_focus,
                        move |window, cx| {
                            cancel_to
                                .update(cx, |panel, cx| panel.show_overview(window, cx))
                                .ok();
                        },
                    ))
                    .child(form::button(
                        theme,
                        "github-follow",
                        if working { "Saving…" } else { "Follow" },
                        ButtonKind::Primary,
                        ready,
                        &self.confirm_focus,
                        move |_, cx| {
                            follow_to.update(cx, |panel, cx| panel.follow(cx)).ok();
                        },
                    )),
            )
            .into_any_element()
    }

    fn confirm_disconnect(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let cancel_to = this.clone();
        let go_to = this;
        let kind = ActionKind::DisconnectGithub(self.workspace);
        let state = self.action(kind, cx);
        let working = state == ActionState::InFlight;
        let error = match state {
            ActionState::Failed(message) => Some(message),
            _ => None,
        };
        let owner = self
            .status(cx)
            .and_then(|s| s.connection)
            .map(|c| c.owner)
            .unwrap_or_else(|| "the organization".to_owned());
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.header(theme, cx))
            .child(
                div()
                    .debug_selector(|| "github-disconnect-question".into())
                    .text_size(theme.type_scale.body)
                    .text_color(theme.colors.text)
                    .child(format!(
                        "Disconnect GitHub? Lazy forgets the app's keys and stops posting; every \
                         channel stops following its repositories and the GitHub app leaves them. \
                         What it posted stays. The app itself stays on GitHub until an owner of \
                         {owner} deletes it there."
                    )),
            )
            .children(error.map(|message| self.error_line(theme, message)))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "github-disconnect-cancel",
                        "Cancel",
                        ButtonKind::Secondary,
                        !working,
                        &self.cancel_focus,
                        move |window, cx| {
                            cancel_to
                                .update(cx, |panel, cx| panel.show_overview(window, cx))
                                .ok();
                        },
                    ))
                    .child(form::button(
                        theme,
                        "github-disconnect-confirm",
                        if working {
                            "Disconnecting…"
                        } else {
                            "Disconnect"
                        },
                        ButtonKind::Danger,
                        !working,
                        &self.confirm_focus,
                        move |_, cx| {
                            go_to
                                .update(cx, |panel, cx| {
                                    let kind = ActionKind::DisconnectGithub(panel.workspace);
                                    panel.waiting = Some((kind, false));
                                    panel.dispatch(Input::DisconnectGithub(panel.workspace), cx);
                                })
                                .ok();
                        },
                    )),
            )
            .into_any_element()
    }
}

impl Render for GithubPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        div()
            .debug_selector(|| "github-panel".into())
            .child(match self.view {
                GithubView::Overview => self.overview(&theme, window, cx),
                GithubView::Adding => self.adding(&theme, window, cx),
                GithubView::ConfirmDisconnect => self.confirm_disconnect(&theme, cx),
            })
    }
}
