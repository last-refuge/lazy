//! A webhook connector (Linear, Jira) inside the Integrations dialog (admins and owners, D34):
//! connecting, the address and signing secret to join up in the provider's settings, which
//! channels follow which team or project, recent deliveries with Retry, and disconnecting.
//!
//! # Views
//!
//! - **Overview**: not available (no credentials key on the server), not connected (Connect), or
//!   connected. Connected shows the setup steps (the address with Copy, what to do in the
//!   provider, and the secret: pasted from Linear, or made here for Jira and shown once), then the
//!   subscriptions with Remove and Add a channel, recent deliveries with Retry, and Disconnect….
//! - **Adding**: the channel, the team or project key (empty follows everything), and which kinds
//!   of activity, then Follow.
//! - **Confirm disconnect**.
//!
//! # Keyboard
//!
//! One tab stop per control, in the order drawn; Tab and Shift+Tab in the text fields move on
//! through the dialog. Enter in the secret saves it; Enter in the key follows. Escape steps back
//! one view; from the overview the dialog returns to its list.

use std::collections::HashMap;

use gpui::{
    AnyElement, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    IntoElement, ParentElement, Render, Styled, Subscription, Window, div, prelude::*, px,
};
use jiff::Zoned;
use jiff::tz::TimeZone;
use lazy_client_core::{ActionKind, ActionState, Input, SecretUrl};
use lazy_domain::connector::{ConnectorProvider, SecretSource};
use lazy_domain::{ConnectorSubscriptionId, ConversationId, ConversationKind, WorkspaceId};
use lazy_protocol::{ConnectorDelivery, ConnectorStatusResponse, GithubDeliveryState};

use super::integration_ui as ui;
use super::single_line_field;
use crate::app_model::{AppEvent, AppModel};
use crate::fonts;
use crate::form::{self, ButtonKind};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::Theme;
use crate::time_fmt::{ago_label, to_zoned};

/// What the panel asks of the dialog around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorPanelEvent {
    /// Tab or Shift+Tab was pressed in one of the panel's text fields.
    Tab { backwards: bool },
    /// Back to the dialog's list of integrations.
    Leave,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorView {
    Overview,
    Adding,
    ConfirmDisconnect,
}

pub struct ConnectorPanel {
    model: Entity<AppModel>,
    workspace: WorkspaceId,
    provider: ConnectorProvider,
    view: ConnectorView,
    secret: Entity<TextInput>,
    scope: Entity<TextInput>,
    channel: Option<ConversationId>,
    events: Vec<String>,
    /// The secret field is showing though one is set (Replace secret was chosen).
    replacing: bool,
    /// The setup steps are showing on a connected view.
    show_steps: bool,
    /// What was last copied, to say so on its button.
    copied: Option<&'static str>,
    /// A request whose end moves the view, and whether it has been seen starting.
    waiting: Option<(ActionKind, bool)>,
    zone: TimeZone,
    back_focus: FocusHandle,
    connect_focus: FocusHandle,
    refresh_focus: FocusHandle,
    copy_address_focus: FocusHandle,
    secret_focus: FocusHandle,
    copy_secret_focus: FocusHandle,
    steps_focus: FocusHandle,
    add_focus: FocusHandle,
    disconnect_focus: FocusHandle,
    cancel_focus: FocusHandle,
    confirm_focus: FocusHandle,
    remove_focus: HashMap<ConnectorSubscriptionId, FocusHandle>,
    retry_focus: HashMap<i64, FocusHandle>,
    channel_focus: HashMap<ConversationId, FocusHandle>,
    event_focus: HashMap<&'static str, FocusHandle>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ConnectorPanelEvent> for ConnectorPanel {}

impl ConnectorPanel {
    pub fn new(
        model: Entity<AppModel>,
        workspace: WorkspaceId,
        provider: ConnectorProvider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let secret = single_line_field("Paste the signing secret", "Signing secret", 256, cx);
        let (placeholder, label) = match provider {
            ConnectorProvider::Linear => ("e.g. ENG (empty: every team)", "Team key"),
            ConnectorProvider::Jira => ("e.g. WEB (empty: every project)", "Project key"),
            ConnectorProvider::Sentry => ("e.g. web-app (empty: every project)", "Project slug"),
            ConnectorProvider::PagerDuty => ("e.g. PXYZ123 (empty: every service)", "Service ID"),
            ConnectorProvider::Unknown => ("", "Key"),
        };
        let scope = single_line_field(placeholder, label, 200, cx);
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &secret,
            window,
            |this, _input, event: &TextInputEvent, _window, cx| match event {
                TextInputEvent::Submit => this.save_secret(cx),
                TextInputEvent::Tab { backwards } => cx.emit(ConnectorPanelEvent::Tab {
                    backwards: *backwards,
                }),
                TextInputEvent::Changed => cx.notify(),
                _ => {}
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &scope,
            window,
            |this, _input, event: &TextInputEvent, _window, cx| match event {
                TextInputEvent::Submit => this.follow(cx),
                TextInputEvent::Tab { backwards } => cx.emit(ConnectorPanelEvent::Tab {
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
            model.dispatch(Input::DismissConnectorSecret, cx);
            model.dispatch(
                Input::LoadConnector {
                    workspace,
                    provider,
                },
                cx,
            );
        });
        Self {
            model,
            workspace,
            provider,
            view: ConnectorView::Overview,
            secret,
            scope,
            channel: None,
            events: default_events(provider),
            replacing: false,
            show_steps: false,
            copied: None,
            waiting: None,
            zone: TimeZone::system(),
            back_focus: cx.focus_handle().tab_stop(true),
            connect_focus: cx.focus_handle().tab_stop(true),
            refresh_focus: cx.focus_handle().tab_stop(true),
            copy_address_focus: cx.focus_handle().tab_stop(true),
            secret_focus: cx.focus_handle().tab_stop(true),
            copy_secret_focus: cx.focus_handle().tab_stop(true),
            steps_focus: cx.focus_handle().tab_stop(true),
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

    pub fn view(&self) -> ConnectorView {
        self.view
    }

    pub fn provider(&self) -> ConnectorProvider {
        self.provider
    }

    /// Where the keyboard starts when the panel opens.
    pub fn first_focus(&self) -> FocusHandle {
        self.back_focus.clone()
    }

    pub fn secret_input(&self) -> &Entity<TextInput> {
        &self.secret
    }

    pub fn scope_input(&self) -> &Entity<TextInput> {
        &self.scope
    }

    pub fn composing(&self, cx: &gpui::App) -> bool {
        self.secret.read(cx).is_composing() || self.scope.read(cx).is_composing()
    }

    /// The action kinds the panel may have left failed, to clear when the dialog closes.
    pub fn kinds(&self) -> Vec<ActionKind> {
        let (w, p) = (self.workspace, self.provider);
        let mut kinds = vec![
            ActionKind::LoadConnector(w, p),
            ActionKind::ConnectConnector(w, p),
            ActionKind::SetConnectorSecret(w, p),
            ActionKind::DisconnectConnector(w, p),
        ];
        kinds.extend(self.channel.map(|c| ActionKind::ConnectorSubscribe(c, p)));
        kinds.extend(
            self.remove_focus
                .keys()
                .map(|id| ActionKind::ConnectorUnsubscribe(*id)),
        );
        kinds.extend(
            self.retry_focus
                .keys()
                .map(|id| ActionKind::RetryConnectorDelivery(*id)),
        );
        kinds
    }

    /// Escape: back one view. Returns whether it did anything (from the overview it does not).
    pub fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.composing(cx) {
            return true;
        }
        match self.view {
            ConnectorView::Overview => false,
            ConnectorView::Adding | ConnectorView::ConfirmDisconnect => {
                self.show_overview(window, cx);
                true
            }
        }
    }

    /// Forgets a secret this panel made (the dialog is closing).
    pub fn forget_secret(&mut self, cx: &mut Context<Self>) {
        self.dispatch(Input::DismissConnectorSecret, cx);
        self.secret.update(cx, |input, cx| input.set_text("", cx));
    }

    // ----- Following the core -----

    fn status(&self, cx: &gpui::App) -> Option<ConnectorStatusResponse> {
        self.model
            .read(cx)
            .core()
            .connector(self.workspace, self.provider)
            .cloned()
    }

    fn made_secret(&self, cx: &gpui::App) -> Option<SecretUrl> {
        self.model
            .read(cx)
            .core()
            .connector_secret(self.provider)
            .cloned()
    }

    fn action(&self, kind: ActionKind, cx: &gpui::App) -> ActionState {
        self.model.read(cx).core().action(kind).clone()
    }

    fn follow_core(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((kind, started)) = self.waiting {
            match self.action(kind, cx) {
                ActionState::InFlight => self.waiting = Some((kind, true)),
                ActionState::Idle if started => {
                    self.waiting = None;
                    match kind {
                        ActionKind::ConnectorSubscribe(..)
                        | ActionKind::DisconnectConnector(..) => self.show_overview(window, cx),
                        ActionKind::SetConnectorSecret(..) => {
                            // A pasted secret is not kept here once the server has it.
                            self.replacing = false;
                            self.secret.update(cx, |input, cx| input.set_text("", cx));
                        }
                        _ => {}
                    }
                }
                ActionState::Failed(_) => self.waiting = None,
                ActionState::Idle => {}
            }
        }
    }

    fn dispatch(&self, input: Input, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
    }

    // ----- Moving and acting -----

    fn show_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.view = ConnectorView::Overview;
        window.focus(&self.back_focus, cx);
        cx.notify();
    }

    /// The channels activity can be followed in: the person's open channels here, by name.
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
                Input::DismissAction(ActionKind::ConnectorSubscribe(channel, self.provider)),
                cx,
            );
        }
        self.events = default_events(self.provider);
        self.scope.update(cx, |input, cx| input.set_text("", cx));
        self.view = ConnectorView::Adding;
        let handle = self.scope.focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        self.dispatch(
            Input::ConnectConnector {
                workspace: self.workspace,
                provider: self.provider,
            },
            cx,
        );
    }

    fn save_secret(&mut self, cx: &mut Context<Self>) {
        let kind = ActionKind::SetConnectorSecret(self.workspace, self.provider);
        if self.action(kind, cx) == ActionState::InFlight {
            return;
        }
        let text = self.secret.read(cx).text().to_owned();
        self.waiting = Some((kind, false));
        self.dispatch(
            Input::SetConnectorSecret {
                workspace: self.workspace,
                provider: self.provider,
                secret: SecretUrl::new(text),
            },
            cx,
        );
    }

    fn generate_secret(&mut self, cx: &mut Context<Self>) {
        self.copied = None;
        self.dispatch(
            Input::GenerateConnectorSecret {
                workspace: self.workspace,
                provider: self.provider,
            },
            cx,
        );
    }

    fn follow(&mut self, cx: &mut Context<Self>) {
        let Some(conversation) = self.channel else {
            return;
        };
        let kind = ActionKind::ConnectorSubscribe(conversation, self.provider);
        if self.action(kind, cx) == ActionState::InFlight {
            return;
        }
        let scope = self.scope.read(cx).text().trim().to_owned();
        self.waiting = Some((kind, false));
        self.dispatch(
            Input::ConnectorSubscribe {
                conversation,
                provider: self.provider,
                scope,
                events: self.events.clone(),
            },
            cx,
        );
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.dispatch(
            Input::LoadConnector {
                workspace: self.workspace,
                provider: self.provider,
            },
            cx,
        );
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

    fn selector(&self, what: &str) -> String {
        format!("{}-{what}", self.provider.as_str())
    }

    fn note(&self, theme: &Theme, what: &str, text: impl Into<String>) -> AnyElement {
        let selector = self.selector(what);
        div()
            .debug_selector(move || selector.clone())
            .text_size(theme.type_scale.small)
            .text_color(theme.colors.text_muted)
            .child(text.into())
            .into_any_element()
    }

    fn error_line(&self, theme: &Theme, message: String) -> AnyElement {
        let selector = self.selector("error");
        div()
            .debug_selector(move || selector.clone())
            .text_size(theme.type_scale.small)
            .text_color(theme.colors.danger)
            .child(message)
            .into_any_element()
    }

    fn section_title(&self, theme: &Theme, text: impl Into<String>) -> AnyElement {
        ui::section(theme, text, None)
    }

    /// Text to copy, in a box, with its Copy button.
    fn copyable(
        &self,
        theme: &Theme,
        what: &'static str,
        text: String,
        focus: &FocusHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let copy_to = cx.entity().downgrade();
        let copied = self.copied == Some(what);
        let selector = self.selector(what);
        let shown = text.clone();
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .debug_selector(move || selector.clone())
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .py_1p5()
                    .rounded(theme.radii.small)
                    .bg(colors.code_surface)
                    .font_family(fonts::MONO_FAMILY)
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text)
                    .overflow_hidden()
                    .child(shown),
            )
            .child(form::button(
                theme,
                self.selector(&format!("copy-{what}")),
                if copied { "Copied" } else { "Copy" },
                ButtonKind::Secondary,
                true,
                focus,
                move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                    copy_to
                        .update(cx, |panel, cx| {
                            panel.copied = Some(what);
                            cx.notify();
                        })
                        .ok();
                },
            ))
            .into_any_element()
    }

    /// Where it stands, for the header's chip.
    fn state_chip(&self, status: Option<&ConnectorStatusResponse>, theme: &Theme) -> AnyElement {
        let (text, tone) = match status.and_then(|s| s.connection.as_ref()) {
            None => ("Not connected", ui::Tone::Quiet),
            Some(c) if !c.secret_set => ("Setup needed", ui::Tone::Attention),
            Some(_) => ("Connected", ui::Tone::Good),
        };
        ui::chip(theme, text.to_owned(), tone)
    }

    fn header(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let leave_to = cx.entity().downgrade();
        let (glyph, color) = ui::marks::of(self.provider);
        let status = self.status(cx);
        ui::header(
            theme,
            self.selector("back"),
            "‹ All integrations",
            &self.back_focus,
            move |_, cx| {
                leave_to
                    .update(cx, |_, cx| cx.emit(ConnectorPanelEvent::Leave))
                    .ok();
            },
            Some(ui::service_mark(theme, glyph, color, 36.)),
            self.provider.label().to_owned(),
            Some(self.state_chip(status.as_ref(), theme)),
        )
    }

    /// The setup steps: the address, what to do in the provider, and the secret.
    fn steps(
        &mut self,
        connection: &lazy_protocol::ConnectorConnection,
        with_secret: bool,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.provider;
        let copy_address_focus = self.copy_address_focus.clone();
        let address = self.copyable(
            theme,
            "address",
            connection.webhook_url.clone(),
            &copy_address_focus,
            cx,
        );
        let mut steps = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(ui::step(
                theme,
                1,
                false,
                "Copy this webhook address",
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .child(self.note(theme, "step-address", step_address(p)))
                    .child(address)
                    .into_any_element(),
            ))
            .child(ui::step(
                theme,
                2,
                false,
                format!("Create the webhook in {}", p.label()),
                ui::bullets(theme, provider_steps(p)),
            ));
        if with_secret {
            let secret = self.secret_step(connection, theme, window, cx);
            steps = steps.child(ui::step(
                theme,
                3,
                connection.secret_set,
                match p.secret_source() {
                    SecretSource::Provider => format!("Paste {}'s signing secret", p.label()),
                    SecretSource::Lazy => format!("Give {} a secret", p.label()),
                },
                secret,
            ));
        }
        steps.into_any_element()
    }

    fn overview(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let (w, p) = (self.workspace, self.provider);
        let load = ActionKind::LoadConnector(w, p);
        let loading = self.action(load, cx) == ActionState::InFlight;
        let load_error = match self.action(load, cx) {
            ActionState::Failed(message) => Some(message),
            _ => None,
        };
        let mut body = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(self.header(theme, cx));
        let refresh_to = this.clone();
        let refresh = form::button(
            theme,
            self.selector("refresh"),
            if loading { "Refreshing…" } else { "Refresh" },
            ButtonKind::Ghost,
            !loading,
            &self.refresh_focus,
            move |_, cx| {
                refresh_to.update(cx, |panel, cx| panel.refresh(cx)).ok();
            },
        );
        let Some(status) = self.status(cx) else {
            body = body
                .children(load_error.map(|m| {
                    self.error_line(theme, format!("{}'s status didn't load. {m}", p.label()))
                }))
                .when(loading, |b| {
                    b.child(self.note(theme, "loading", "Loading…"))
                })
                .child(div().flex().child(refresh));
            return body.into_any_element();
        };
        if !status.available {
            return body
                .child(self.note(
                    theme,
                    "unavailable",
                    "This server can't hold signing secrets yet. Whoever runs it needs to set \
                     LAZY_CREDENTIALS_KEY (see DEPLOY.md, \"The credentials key\") and restart it.",
                ))
                .child(div().flex().child(refresh))
                .into_any_element();
        }
        let Some(connection) = status.connection.clone() else {
            // Not connected: what it does, what it posts, and Connect.
            let kind = ActionKind::ConnectConnector(w, p);
            let working = self.action(kind, cx) == ActionState::InFlight;
            let error = match self.action(kind, cx) {
                ActionState::Failed(message) => Some(message),
                _ => None,
            };
            let connect_to = this.clone();
            let kinds: Vec<&str> = p.kinds().iter().map(|k| k.label).collect();
            return body
                .child(self.note(theme, "intro", intro(p)))
                .child(ui::section(theme, "What it can post", None))
                .child(ui::bullets(theme, &kinds))
                .children(error.map(|m| self.error_line(theme, m)))
                .child(div().flex().child(form::button(
                    theme,
                    self.selector("connect"),
                    if working {
                        "Connecting…".to_owned()
                    } else {
                        format!("Connect {}", p.label())
                    },
                    ButtonKind::Primary,
                    !working,
                    &self.connect_focus,
                    move |_, cx| {
                        connect_to.update(cx, |panel, cx| panel.connect(cx)).ok();
                    },
                )))
                .into_any_element();
        };

        if !connection.secret_set {
            // Setting up: three steps, then channels.
            return body
                .child(self.note(
                    theme,
                    "setup-intro",
                    format!(
                        "Three steps join {} to Lazy. Then choose which channels it posts in.",
                        p.label()
                    ),
                ))
                .child(self.steps(&connection, true, theme, window, cx))
                .child(ui::divider(theme))
                .child(self.disconnect_row(theme, cx))
                .into_any_element();
        }

        // Connected: what it posts where comes first; the connection itself last.
        let add_to = this.clone();
        body = body
            .child(ui::section(
                theme,
                format!("Channels following {}", p.label()),
                None,
            ))
            .child(self.subscriptions(&status, theme, cx))
            .child(div().flex().child(form::button(
                theme,
                self.selector("add"),
                "Add a channel",
                ButtonKind::Primary,
                !self.channels(cx).is_empty(),
                &self.add_focus,
                move |window, cx| {
                    add_to
                        .update(cx, |panel, cx| panel.start_adding(window, cx))
                        .ok();
                },
            )))
            .child(ui::divider(theme))
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .child(ui::section(theme, "Recent deliveries", None)),
                    )
                    .child(refresh),
            )
            .child(self.deliveries(&status.deliveries, theme, cx))
            .child(ui::divider(theme))
            .child(ui::section(
                theme,
                "Connection",
                Some(format!(
                    "{} sends its activity to this address, signed with the secret.",
                    p.label()
                )),
            ));
        let copy_address_focus = self.copy_address_focus.clone();
        body = body.child(self.copyable(
            theme,
            "address",
            connection.webhook_url.clone(),
            &copy_address_focus,
            cx,
        ));
        body = body.child(self.secret_step(&connection, theme, window, cx));
        let steps_to = this;
        body = body.child(div().flex().ml(px(-12.)).child(form::button(
            theme,
            self.selector("show-steps"),
            if self.show_steps {
                "Hide setup steps"
            } else {
                "Show setup steps"
            },
            ButtonKind::Ghost,
            true,
            &self.steps_focus,
            move |_, cx| {
                steps_to
                    .update(cx, |panel, cx| {
                        panel.show_steps = !panel.show_steps;
                        cx.notify();
                    })
                    .ok();
            },
        )));
        if self.show_steps {
            body = body.child(self.steps(&connection, false, theme, window, cx));
        }
        body.child(ui::divider(theme))
            .child(self.disconnect_row(theme, cx))
            .into_any_element()
    }

    /// Disconnect…, at the foot of the view.
    fn disconnect_row(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let disconnect_to = cx.entity().downgrade();
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(div().flex_1().child(self.note(
                theme,
                "disconnect-hint",
                format!(
                    "Disconnecting stops posting and forgets the secret. What {} posted stays.",
                    self.provider.label()
                ),
            )))
            .child(form::button(
                theme,
                self.selector("disconnect"),
                "Disconnect…",
                ButtonKind::Danger,
                true,
                &self.disconnect_focus,
                move |window, cx| {
                    disconnect_to
                        .update(cx, |panel, cx| {
                            panel.dispatch(
                                Input::DismissAction(ActionKind::DisconnectConnector(
                                    panel.workspace,
                                    panel.provider,
                                )),
                                cx,
                            );
                            panel.view = ConnectorView::ConfirmDisconnect;
                            window.focus(&panel.cancel_focus, cx);
                            cx.notify();
                        })
                        .ok();
                },
            ))
            .into_any_element()
    }

    /// The signing secret: pasted from the provider, or made here and shown once.
    fn secret_step(
        &mut self,
        connection: &lazy_protocol::ConnectorConnection,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let p = self.provider;
        let kind = ActionKind::SetConnectorSecret(self.workspace, p);
        let state = self.action(kind, cx);
        let working = state == ActionState::InFlight;
        let error = match &state {
            ActionState::Failed(message) => Some(message.clone()),
            _ => None,
        };
        match p.secret_source() {
            SecretSource::Provider => {
                if connection.secret_set && !self.replacing {
                    let replace_to = this;
                    return div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(ui::chip(theme, "Secret saved".to_owned(), ui::Tone::Good))
                        .child(div().flex_1().child(self.note(
                            theme,
                            "secret-set",
                            "Deliveries are checked against it.",
                        )))
                        .child(form::button(
                            theme,
                            self.selector("replace-secret"),
                            "Replace secret",
                            ButtonKind::Secondary,
                            true,
                            &self.secret_focus,
                            move |window, cx| {
                                replace_to
                                    .update(cx, |panel, cx| {
                                        panel.replacing = true;
                                        let handle = panel.secret.focus_handle(cx);
                                        window.focus(&handle, cx);
                                        cx.notify();
                                    })
                                    .ok();
                            },
                        ))
                        .into_any_element();
                }
                let ready = !working && !self.secret.read(cx).text().trim().is_empty();
                let save_to = this;
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(form::labeled_field(
                        theme,
                        "Signing secret",
                        &self.secret,
                        Some(format!(
                            "From the webhook's page in {}. Lazy keeps it sealed and never shows \
                             it again.",
                            p.label()
                        )
                        .into()),
                        error.map(Into::into),
                        window,
                        cx,
                    ))
                    .child(div().flex().child(form::button(
                        theme,
                        self.selector("save-secret"),
                        if working { "Saving…" } else { "Save secret" },
                        ButtonKind::Primary,
                        ready,
                        &self.copy_secret_focus,
                        move |_, cx| {
                            save_to.update(cx, |panel, cx| panel.save_secret(cx)).ok();
                        },
                    )))
                    .into_any_element()
            }
            SecretSource::Lazy => {
                let made = self.made_secret(cx);
                let generate_to = this;
                let mut step = div().flex().flex_col().gap_2();
                if let Some(made) = made {
                    let focus = self.copy_secret_focus.clone();
                    step = step
                        .child(self.note(
                            theme,
                            "secret-made",
                            format!(
                                "Paste this into the webhook's Secret field in {}. It is shown \
                                 only now.",
                                p.label()
                            ),
                        ))
                        .child(self.copyable(
                            theme,
                            "secret",
                            made.as_str().to_owned(),
                            &focus,
                            cx,
                        ));
                } else if connection.secret_set {
                    step = step.child(self.note(
                        theme,
                        "secret-set",
                        "Secret set. Lost it? Make a new one and paste it into the webhook again; \
                         the old one stops working at once.",
                    ));
                }
                step.children(error.map(|m| self.error_line(theme, m)))
                    .child(div().flex().child(form::button(
                        theme,
                        self.selector("generate-secret"),
                        if working {
                            "Making…"
                        } else if connection.secret_set {
                            "Make a new secret"
                        } else {
                            "Make a secret"
                        },
                        if connection.secret_set {
                            ButtonKind::Secondary
                        } else {
                            ButtonKind::Primary
                        },
                        !working,
                        &self.secret_focus,
                        move |_, cx| {
                            generate_to
                                .update(cx, |panel, cx| panel.generate_secret(cx))
                                .ok();
                        },
                    )))
                    .into_any_element()
            }
        }
    }

    fn subscriptions(
        &mut self,
        status: &ConnectorStatusResponse,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let p = self.provider;
        if status.subscriptions.is_empty() {
            return self.note(
                theme,
                "no-subscriptions",
                format!("No channel follows {} yet. Add one to start.", p.label()),
            );
        }
        let this = cx.entity().downgrade();
        let list_selector = self.selector("subscriptions");
        let mut list = div()
            .id(gpui::SharedString::from(self.selector("subscriptions")))
            .debug_selector(move || list_selector.clone())
            .max_h(px(200.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1();
        for (index, sub) in status.subscriptions.iter().enumerate() {
            let id = sub.id;
            let focus = Self::handle_for(&mut self.remove_focus, id, cx);
            let state = self.action(ActionKind::ConnectorUnsubscribe(id), cx);
            let kinds = sub
                .events
                .iter()
                .map(|key| p.kind(key).map(|k| k.label).unwrap_or(key.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            let what = if sub.scope.is_empty() {
                format!("Everything → #{}", sub.channel_name.as_str())
            } else {
                format!("{} → #{}", sub.scope, sub.channel_name.as_str())
            };
            let row_selector = self.selector("subscription");
            let remove_to = this.clone();
            list = list.child(
                div()
                    .debug_selector(move || row_selector.clone())
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
                                    .child(what),
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
                        self.selector(&format!("remove-{index}")),
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
                                    panel.dispatch(Input::ConnectorUnsubscribe(id), cx)
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
        deliveries: &[ConnectorDelivery],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        if deliveries.is_empty() {
            return self.note(
                theme,
                "no-deliveries",
                format!(
                    "Nothing from {} yet. Activity a channel follows shows here as it arrives.",
                    self.provider.label()
                ),
            );
        }
        let this = cx.entity().downgrade();
        let now = Zoned::now();
        let workspace = self.workspace;
        let list_selector = self.selector("deliveries");
        let mut rows = div()
            .id(gpui::SharedString::from(self.selector("deliveries")))
            .debug_selector(move || list_selector.clone())
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
            let when = ago_label(&to_zoned(delivery.received_at, &self.zone), &now);
            let what = if delivery.summary.is_empty() {
                "Activity".to_owned()
            } else {
                delivery.summary.clone()
            };
            let retryable = matches!(
                delivery.state,
                GithubDeliveryState::Failed | GithubDeliveryState::Unknown
            );
            let retry_state = self.action(ActionKind::RetryConnectorDelivery(id), cx);
            let focus = retryable.then(|| Self::handle_for(&mut self.retry_focus, id, cx));
            let row_selector = self.selector("delivery");
            let retry_to = this.clone();
            rows = rows.child(
                div()
                    .debug_selector(move || row_selector.clone())
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
                                    .child(format!("{what} · {when}")),
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
                            self.selector(&format!("retry-{index}")),
                            "Retry",
                            ButtonKind::Secondary,
                            true,
                            &focus,
                            move |_, cx| {
                                retry_to
                                    .update(cx, |panel, cx| {
                                        panel.dispatch(
                                            Input::RetryConnectorDelivery {
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
        let p = self.provider;
        let channels = self.channels(cx);
        let state = self
            .channel
            .map(|c| self.action(ActionKind::ConnectorSubscribe(c, p), cx))
            .unwrap_or(ActionState::Idle);
        let working = state == ActionState::InFlight;
        let error = match &state {
            ActionState::Failed(message) => Some(message.clone()),
            _ => None,
        };
        let ready = self.channel.is_some() && !self.events.is_empty() && !working;
        let mut choices = div()
            .id(gpui::SharedString::from(self.selector("channel-choices")))
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
                self.selector(&format!("channel-{index}")),
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
                                    Input::DismissAction(ActionKind::ConnectorSubscribe(
                                        old,
                                        panel.provider,
                                    )),
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
        for kind in p.kinds() {
            let key = kind.key;
            let focus = Self::handle_for(&mut self.event_focus, key, cx);
            let on = self.events.iter().any(|e| e == key);
            let toggle_to = this.clone();
            kinds = kinds.child(form::button(
                theme,
                self.selector(&format!("event-{key}")),
                if on {
                    format!("✓ {}", kind.label)
                } else {
                    kind.label.to_owned()
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
                            if let Some(at) = panel.events.iter().position(|e| e == key) {
                                panel.events.remove(at);
                            } else {
                                panel.events.push(key.to_owned());
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
                format!("{} key", p.scope_label()),
                &self.scope,
                Some(scope_hint(p).into()),
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
                        self.selector("cancel"),
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
                        self.selector("follow"),
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
        let p = self.provider;
        let kind = ActionKind::DisconnectConnector(self.workspace, p);
        let state = self.action(kind, cx);
        let working = state == ActionState::InFlight;
        let error = match state {
            ActionState::Failed(message) => Some(message),
            _ => None,
        };
        let question_selector = self.selector("disconnect-question");
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.header(theme, cx))
            .child(
                div()
                    .debug_selector(move || question_selector.clone())
                    .text_size(theme.type_scale.body)
                    .text_color(theme.colors.text)
                    .child(format!(
                        "Disconnect {name}? Lazy forgets the signing secret and stops posting; \
                         every channel stops following {name} and its bot leaves them. What it \
                         posted stays. Delete the webhook in {name}'s settings too, or it will \
                         keep trying to deliver.",
                        name = p.label()
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
                        self.selector("disconnect-cancel"),
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
                        self.selector("disconnect-confirm"),
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
                                    let kind = ActionKind::DisconnectConnector(
                                        panel.workspace,
                                        panel.provider,
                                    );
                                    panel.waiting = Some((kind, false));
                                    panel.dispatch(
                                        Input::DisconnectConnector {
                                            workspace: panel.workspace,
                                            provider: panel.provider,
                                        },
                                        cx,
                                    );
                                })
                                .ok();
                        },
                    )),
            )
            .into_any_element()
    }
}

fn default_events(provider: ConnectorProvider) -> Vec<String> {
    provider
        .default_kinds()
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// What connecting does, before it is done.
fn intro(provider: ConnectorProvider) -> String {
    match provider {
        ConnectorProvider::Linear => "Post Linear issues, status changes, comments and project \
             updates into the channels you choose. Connecting gives you an address to paste into a \
             webhook in Linear's settings; you need to be a Linear admin to make one."
            .to_owned(),
        ConnectorProvider::Jira => "Post Jira issues, status changes, assignments and comments \
             into the channels you choose. Connecting gives you an address to paste into a webhook \
             in Jira's settings; you need the Administer Jira permission to make one."
            .to_owned(),
        ConnectorProvider::Sentry => "Post new and returning Sentry issues and alerts into the \
             channels you choose. Connecting gives you an address for an internal integration in \
             Sentry's settings; you need to be a Sentry manager or owner to make one."
            .to_owned(),
        ConnectorProvider::PagerDuty => "Post PagerDuty incidents (triggered, acknowledged, \
             resolved) into the channels you choose. Connecting gives you an address for a Generic \
             Webhook in PagerDuty; you need a Manager, Admin or Owner role to add one."
            .to_owned(),
        ConnectorProvider::Unknown => String::new(),
    }
}

fn step_address(provider: ConnectorProvider) -> String {
    format!(
        "It's where {} sends its activity. Only Lazy can read what arrives there.",
        provider.label()
    )
}

/// What to do in the provider, a line each.
fn provider_steps(provider: ConnectorProvider) -> &'static [&'static str] {
    match provider {
        ConnectorProvider::Linear => &[
            "In Linear, open Settings, API, Webhooks, and choose New webhook.",
            "Paste the address and label it \"Lazy\".",
            "Choose all public teams, or one team.",
            "Tick Issues, Comments, Projects and Project updates, then save.",
            "Open the webhook again and copy its signing secret.",
        ],
        ConnectorProvider::Jira => &[
            "In Jira, open Settings, System, WebHooks, and choose Create a WebHook.",
            "Paste the address as the URL.",
            "Paste the secret from the next step into the Secret field.",
            "Under Events, tick Issue created and updated, and Comment created.",
            "Optionally narrow it to some projects with JQL, then save.",
        ],
        ConnectorProvider::Sentry => &[
            "In Sentry, open Settings, Developer Settings, and create an internal integration named Lazy.",
            "Paste the address as its Webhook URL and turn on Alert Action.",
            "Give it Read access to Issue & Event and tick the issue webhook, then save.",
            "Copy its Client Secret.",
            "For alerts, add \"Send a notification via Lazy\" to an alert rule.",
        ],
        ConnectorProvider::PagerDuty => &[
            "In PagerDuty, open Integrations, Generic Webhooks (v3), and choose New Webhook (or use a service's Integrations tab).",
            "Paste the address as the Webhook URL.",
            "Choose the incident events: triggered, acknowledged, resolved, escalated, reassigned, annotated.",
            "Add it, and copy the secret PagerDuty shows: it is shown only once.",
        ],
        ConnectorProvider::Unknown => &[],
    }
}

fn scope_hint(provider: ConnectorProvider) -> String {
    match provider {
        ConnectorProvider::Linear => {
            "The team's key, as in its issue ids (ENG for ENG-12), or the \
             team's Linear address. Leave it empty to follow every team the webhook sends."
                .to_owned()
        }
        ConnectorProvider::Jira => {
            "The project's key, as in its issue keys (WEB for WEB-42), or an \
             issue's Jira address. Leave it empty to follow every project the webhook sends."
                .to_owned()
        }
        ConnectorProvider::Sentry => "The project's slug, or its Sentry address. Leave it empty to \
             follow every project. Issue alerts name no project, so only a channel that follows \
             everything gets them."
            .to_owned(),
        ConnectorProvider::PagerDuty => "The service's ID (as in its address, \
             .../service-directory/PXYZ123) or its address. Leave it empty to follow every service. \
             Notes name no service, so only a channel that follows everything gets them."
            .to_owned(),
        ConnectorProvider::Unknown => String::new(),
    }
}

impl Render for ConnectorPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let selector = self.selector("panel");
        div()
            .debug_selector(move || selector.clone())
            .child(match self.view {
                ConnectorView::Overview => self.overview(&theme, window, cx),
                ConnectorView::Adding => self.adding(&theme, window, cx),
                ConnectorView::ConfirmDisconnect => self.confirm_disconnect(&theme, cx),
            })
    }
}
