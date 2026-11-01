//! The workspace's integrations (admins and owners, D30): every incoming webhook in a channel the
//! person can see, how each is doing, and everything an admin does with one: add it to a channel,
//! read its delivery log, give it a new address, and delete it.
//!
//! # Views
//!
//! - **List**: each webhook with its channel, who added it, its latest delivery, and how many of
//!   its requests failed in the last day; Manage on each; Add incoming webhook.
//! - **Adding**: which channel (the channels the person is in), and the app's name.
//! - **Details**: one webhook's delivery log (newest first, what the server keeps), with New
//!   address… and Delete….
//! - **Address**: a new address, shown once (after adding, or after New address), with Copy and
//!   Done. The core holds it only until [`Input::DismissCreatedWebhook`], which is sent the moment
//!   the view is left by any route, and the field is emptied with it.
//! - **Confirm**: before a new address (the old one stops working) and before deleting.
//!
//! Lazy keeps nothing that was posted to a webhook, so it cannot send a failed request again; the
//! log says what happened and the sender retries (with the same `Idempotency-Key`, a retry posts
//! once). Retrying from here comes with integrations that Lazy delivers itself.
//!
//! # Keyboard
//!
//! One tab stop per control, in the order drawn. Enter in the name creates. Escape steps back one
//! view (and does nothing while an input method is composing); from the list it closes the
//! dialog.

use std::collections::HashMap;

use gpui::{
    AnyElement, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    Hsla, IntoElement, ParentElement, Render, ScrollHandle, Styled, Subscription, Window, div,
    prelude::*, px,
};
use jiff::Zoned;
use jiff::tz::TimeZone;
use lazy_client_core::{ActionKind, ActionState, Input};
use lazy_domain::{ConversationId, ConversationKind, MAX_WEBHOOKS_PER_CONVERSATION, WebhookId};
use lazy_domain::{MAX_WEBHOOK_DELIVERIES_KEPT, WorkspaceId};
use lazy_protocol::{WebhookDelivery, WebhookDeliveryOutcome, WorkspaceWebhook};

use super::apps_panel::{AppsPanel, AppsPanelEvent};
use super::connector_panel::{ConnectorPanel, ConnectorPanelEvent};
use super::github_panel::{GithubPanel, GithubPanelEvent};
use super::integration_ui as ui;
use super::{any_composing, single_line_field};
use crate::app_model::{AppEvent, AppModel};
use crate::fonts;
use crate::form::{self, ButtonKind};
use crate::modal::{DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, dialog_overlay};
use crate::states;
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::Theme;
use crate::time_fmt::{ago_label, to_zoned};
use lazy_domain::connector::ConnectorProvider;

/// Which view of the dialog is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationsView {
    List,
    Adding,
    Details(WebhookId),
    /// A new address, shown once. `back` is the webhook whose details to return to (after New
    /// address), or `None` for the list (after adding).
    Address {
        back: Option<WebhookId>,
    },
    ConfirmNewAddress(WebhookId),
    ConfirmDelete(WebhookId),
    /// GitHub (D32), drawn by [`GithubPanel`].
    Github,
    /// A webhook connector (D34), drawn by [`ConnectorPanel`].
    Connector(ConnectorProvider),
    /// Apps with bot tokens (D35), drawn by [`AppsPanel`].
    Apps,
}

pub struct IntegrationsDialog {
    model: Entity<AppModel>,
    trap: ModalTrap,
    workspace: WorkspaceId,
    view: IntegrationsView,
    name: Entity<TextInput>,
    /// Holds a new address while it is shown, read-only; emptied when it is dismissed.
    address: Entity<TextInput>,
    /// The channel a new webhook is for.
    channel: Option<ConversationId>,
    /// The request in progress whose end moves the view, and whether it has been seen starting.
    waiting: Option<(ActionKind, bool)>,
    copied: bool,
    zone: TimeZone,
    add_focus: FocusHandle,
    retry_focus: FocusHandle,
    cancel_focus: FocusHandle,
    create_focus: FocusHandle,
    copy_focus: FocusHandle,
    done_focus: FocusHandle,
    back_focus: FocusHandle,
    refresh_focus: FocusHandle,
    new_address_focus: FocusHandle,
    delete_focus: FocusHandle,
    confirm_cancel_focus: FocusHandle,
    confirm_focus: FocusHandle,
    manage_focus: HashMap<WebhookId, FocusHandle>,
    channel_focus: HashMap<ConversationId, FocusHandle>,
    list_scroll: ScrollHandle,
    log_scroll: ScrollHandle,
    github_focus: FocusHandle,
    /// The GitHub view, made the first time it is opened.
    github: Option<Entity<GithubPanel>>,
    /// Each connector's card button.
    connector_focus: HashMap<ConnectorProvider, FocusHandle>,
    /// Each connector's view, made the first time it is opened.
    connectors: HashMap<ConnectorProvider, Entity<ConnectorPanel>>,
    apps_focus: FocusHandle,
    /// The apps view, made the first time it is opened.
    apps: Option<Entity<AppsPanel>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for IntegrationsDialog {}

impl IntegrationsDialog {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace = model
            .read(cx)
            .core()
            .primary_workspace()
            .map(|info| info.workspace.id)
            .unwrap_or_else(WorkspaceId::generate);
        let name = single_line_field("e.g. Deploy bot", "Name of the app", 64, cx);
        let address = single_line_field("", "Webhook address", 2048, cx);
        address.update(cx, |input, cx| input.set_enabled(false, cx));
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &name,
            window,
            |this, _input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => this.create(cx),
                TextInputEvent::Tab { backwards } => this.trap.tab(*backwards, window, cx),
                TextInputEvent::Changed => cx.notify(),
                TextInputEvent::UpWhenEmpty
                | TextInputEvent::PasteAttachments
                | TextInputEvent::Suggestion(_) => {}
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &address,
            window,
            |this, _input, event: &TextInputEvent, window, cx| {
                if let TextInputEvent::Tab { backwards } = event {
                    this.trap.tab(*backwards, window, cx);
                }
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
        // An address left over from elsewhere is forgotten, and what the server has is read.
        model.update(cx, |model, cx| {
            if model.core().created_webhook().is_some() {
                model.dispatch(Input::DismissCreatedWebhook, cx);
            }
            model.dispatch(Input::LoadWorkspaceWebhooks(workspace), cx);
            model.dispatch(Input::LoadGithub(workspace), cx);
            for provider in ConnectorProvider::ALL {
                model.dispatch(
                    Input::LoadConnector {
                        workspace,
                        provider,
                    },
                    cx,
                );
            }
            model.dispatch(Input::LoadApps(workspace), cx);
        });
        let mut trap = ModalTrap::new(cx);
        let add_focus = cx.focus_handle().tab_stop(true);
        trap.open(&add_focus, window, cx);
        Self {
            model,
            trap,
            workspace,
            view: IntegrationsView::List,
            name,
            address,
            channel: None,
            waiting: None,
            copied: false,
            zone: TimeZone::system(),
            add_focus,
            retry_focus: cx.focus_handle().tab_stop(true),
            cancel_focus: cx.focus_handle().tab_stop(true),
            create_focus: cx.focus_handle().tab_stop(true),
            copy_focus: cx.focus_handle().tab_stop(true),
            done_focus: cx.focus_handle().tab_stop(true),
            back_focus: cx.focus_handle().tab_stop(true),
            refresh_focus: cx.focus_handle().tab_stop(true),
            new_address_focus: cx.focus_handle().tab_stop(true),
            delete_focus: cx.focus_handle().tab_stop(true),
            confirm_cancel_focus: cx.focus_handle().tab_stop(true),
            confirm_focus: cx.focus_handle().tab_stop(true),
            manage_focus: HashMap::new(),
            channel_focus: HashMap::new(),
            list_scroll: ScrollHandle::new(),
            log_scroll: ScrollHandle::new(),
            github_focus: cx.focus_handle().tab_stop(true),
            github: None,
            connector_focus: ConnectorProvider::ALL
                .into_iter()
                .map(|p| (p, cx.focus_handle().tab_stop(true)))
                .collect(),
            connectors: HashMap::new(),
            apps_focus: cx.focus_handle().tab_stop(true),
            apps: None,
            _subscriptions: subscriptions,
        }
    }

    // ----- For tests -----

    /// Which view is showing.
    pub fn view(&self) -> IntegrationsView {
        self.view
    }

    /// The name box.
    pub fn name_input(&self) -> &Entity<TextInput> {
        &self.name
    }

    /// The box that holds a new address while it is shown.
    pub fn address_input(&self) -> &Entity<TextInput> {
        &self.address
    }

    // ----- Following the core -----

    fn action(&self, kind: ActionKind, cx: &gpui::App) -> ActionState {
        self.model.read(cx).core().action(kind).clone()
    }

    fn entry(&self, id: WebhookId, cx: &gpui::App) -> Option<WorkspaceWebhook> {
        self.model
            .read(cx)
            .core()
            .workspace_webhooks(self.workspace)
            .iter()
            .find(|entry| entry.webhook.id == id)
            .cloned()
    }

    fn follow_core(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Demoted while open: there is nothing left to offer.
        if !self
            .model
            .read(cx)
            .core()
            .can_manage_workspace_webhooks(self.workspace)
        {
            self.finish(window, cx);
            return;
        }
        let created = self
            .model
            .read(cx)
            .core()
            .created_webhook()
            .map(|created| (created.webhook.id, created.webhook.conversation_id));
        match (self.view, created) {
            // The address went away under the view that shows it (its webhook was deleted, or
            // the channel was lost): the field is emptied with it.
            (IntegrationsView::Address { back }, None) => {
                self.leave_address(back, window, cx);
                return;
            }
            (IntegrationsView::Adding, Some((_, conversation)))
                if Some(conversation) == self.channel =>
            {
                self.show_address(None, window, cx);
            }
            (IntegrationsView::ConfirmNewAddress(id), Some((webhook, _))) if webhook == id => {
                self.show_address(Some(id), window, cx);
            }
            _ => {}
        }
        if let Some((kind, started)) = self.waiting {
            match self.action(kind, cx) {
                ActionState::InFlight => self.waiting = Some((kind, true)),
                ActionState::Idle if started => {
                    self.waiting = None;
                    if matches!(kind, ActionKind::DeleteWebhook(_)) {
                        self.show_list(window, cx);
                    }
                }
                ActionState::Failed(_) => self.waiting = None,
                ActionState::Idle => {}
            }
        }
        // A webhook whose details are open was deleted elsewhere (the list no longer has it).
        if let IntegrationsView::Details(id) = self.view {
            let loading = self.action(ActionKind::LoadWorkspaceWebhooks(self.workspace), cx)
                == ActionState::InFlight;
            if !loading && self.entry(id, cx).is_none() {
                self.show_list(window, cx);
            }
        }
    }

    fn dispatch(&self, input: Input, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
    }

    // ----- Moving between views -----

    fn show_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.view = IntegrationsView::List;
        window.focus(&self.add_focus, cx);
        cx.notify();
    }

    /// Opens a webhook's details and reads its log.
    pub fn open_details(&mut self, id: WebhookId, window: &mut Window, cx: &mut Context<Self>) {
        for kind in [
            ActionKind::NewWebhookAddress(id),
            ActionKind::DeleteWebhook(id),
        ] {
            self.dispatch(Input::DismissAction(kind), cx);
        }
        self.dispatch(Input::LoadWebhookDeliveries(id), cx);
        self.view = IntegrationsView::Details(id);
        window.focus(&self.back_focus, cx);
        cx.notify();
    }

    /// Asks whether to give a webhook a new address (what New address… does).
    pub fn show_confirm_new_address(
        &mut self,
        id: WebhookId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(Input::DismissAction(ActionKind::NewWebhookAddress(id)), cx);
        self.view = IntegrationsView::ConfirmNewAddress(id);
        window.focus(&self.confirm_cancel_focus, cx);
        cx.notify();
    }

    /// Asks whether to delete a webhook (what Delete… does).
    pub fn show_confirm_delete(
        &mut self,
        id: WebhookId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(Input::DismissAction(ActionKind::DeleteWebhook(id)), cx);
        self.view = IntegrationsView::ConfirmDelete(id);
        window.focus(&self.confirm_cancel_focus, cx);
        cx.notify();
    }

    /// Opens the form for a new webhook.
    pub fn start_adding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let channels = self.channels(cx);
        // The channel the person is looking at, if it can take one; else the first.
        let selected = self.model.read(cx).core().selected_conversation();
        self.channel = selected
            .filter(|id| channels.iter().any(|(channel, _)| channel == id))
            .or_else(|| channels.first().map(|(id, _)| *id));
        if let Some(channel) = self.channel {
            self.dispatch(Input::DismissAction(ActionKind::CreateWebhook(channel)), cx);
        }
        self.name.update(cx, |input, cx| input.set_text("", cx));
        self.view = IntegrationsView::Adding;
        let handle = self.name.focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn show_address(
        &mut self,
        back: Option<WebhookId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let url = self
            .model
            .read(cx)
            .core()
            .created_webhook()
            .map(|created| created.url.as_str().to_owned());
        let Some(url) = url else {
            return;
        };
        // The caret goes to the start so the field shows where the address begins.
        self.address.update(cx, |input, cx| {
            input.set_text(&url, cx);
            input.set_selection(0, 0, cx);
        });
        self.copied = false;
        self.view = IntegrationsView::Address { back };
        window.focus(&self.copy_focus, cx);
        cx.notify();
    }

    /// The address leaves memory: the core's copy and the box's text.
    fn forget_address(&mut self, cx: &mut Context<Self>) {
        self.address.update(cx, |input, cx| input.set_text("", cx));
        if self.model.read(cx).core().created_webhook().is_some() {
            self.dispatch(Input::DismissCreatedWebhook, cx);
        }
        self.copied = false;
    }

    fn leave_address(
        &mut self,
        back: Option<WebhookId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.forget_address(cx);
        match back.filter(|id| self.entry(*id, cx).is_some()) {
            Some(id) => self.open_details(id, window, cx),
            None => self.show_list(window, cx),
        }
    }

    /// Opens the GitHub view (making it the first time).
    pub fn open_github(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let panel = match &self.github {
            Some(panel) => {
                let workspace = self.workspace;
                self.model.update(cx, |model, cx| {
                    model.dispatch(Input::LoadGithub(workspace), cx)
                });
                panel.clone()
            }
            None => {
                let model = self.model.clone();
                let workspace = self.workspace;
                let panel = cx.new(|cx| GithubPanel::new(model, workspace, window, cx));
                let events = cx.subscribe_in(
                    &panel,
                    window,
                    |this, _panel, event: &GithubPanelEvent, window, cx| match event {
                        GithubPanelEvent::Tab { backwards } => {
                            this.trap.tab(*backwards, window, cx)
                        }
                        GithubPanelEvent::Leave => {
                            this.view = IntegrationsView::List;
                            window.focus(&this.github_focus, cx);
                            cx.notify();
                        }
                    },
                );
                self._subscriptions.push(events);
                self.github = Some(panel.clone());
                panel
            }
        };
        self.view = IntegrationsView::Github;
        let focus = panel.read(cx).first_focus();
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Opens a connector's view (making it the first time).
    pub fn open_connector(
        &mut self,
        provider: ConnectorProvider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = match self.connectors.get(&provider) {
            Some(panel) => {
                let workspace = self.workspace;
                self.model.update(cx, |model, cx| {
                    model.dispatch(
                        Input::LoadConnector {
                            workspace,
                            provider,
                        },
                        cx,
                    )
                });
                panel.clone()
            }
            None => {
                let model = self.model.clone();
                let workspace = self.workspace;
                let panel =
                    cx.new(|cx| ConnectorPanel::new(model, workspace, provider, window, cx));
                let events = cx.subscribe_in(
                    &panel,
                    window,
                    move |this, _panel, event: &ConnectorPanelEvent, window, cx| match event {
                        ConnectorPanelEvent::Tab { backwards } => {
                            this.trap.tab(*backwards, window, cx)
                        }
                        ConnectorPanelEvent::Leave => this.leave_connector(provider, window, cx),
                    },
                );
                self._subscriptions.push(events);
                self.connectors.insert(provider, panel.clone());
                panel
            }
        };
        self.view = IntegrationsView::Connector(provider);
        let focus = panel.read(cx).first_focus();
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Back from a connector's view to the list, on its card.
    fn leave_connector(
        &mut self,
        provider: ConnectorProvider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.connectors.get(&provider).cloned() {
            panel.update(cx, |panel, cx| panel.forget_secret(cx));
        }
        self.view = IntegrationsView::List;
        if let Some(focus) = self.connector_focus.get(&provider) {
            window.focus(focus, cx);
        }
        cx.notify();
    }

    /// Opens the apps view (making it the first time).
    pub fn open_apps(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let panel = match &self.apps {
            Some(panel) => {
                let workspace = self.workspace;
                self.model.update(cx, |model, cx| {
                    model.dispatch(Input::LoadApps(workspace), cx)
                });
                panel.clone()
            }
            None => {
                let model = self.model.clone();
                let workspace = self.workspace;
                let panel = cx.new(|cx| AppsPanel::new(model, workspace, window, cx));
                let events = cx.subscribe_in(
                    &panel,
                    window,
                    |this, _panel, event: &AppsPanelEvent, window, cx| match event {
                        AppsPanelEvent::Tab { backwards } => this.trap.tab(*backwards, window, cx),
                        AppsPanelEvent::Leave => this.leave_apps(window, cx),
                    },
                );
                self._subscriptions.push(events);
                self.apps = Some(panel.clone());
                panel
            }
        };
        self.view = IntegrationsView::Apps;
        let focus = panel.read(cx).first_focus();
        window.focus(&focus, cx);
        cx.notify();
    }

    fn leave_apps(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.apps.clone() {
            panel.update(cx, |panel, cx| panel.forget_token(cx));
        }
        self.view = IntegrationsView::List;
        window.focus(&self.apps_focus, cx);
        cx.notify();
    }

    /// The apps view, while it exists. For tests.
    pub fn apps_panel(&self) -> Option<Entity<AppsPanel>> {
        self.apps.clone()
    }

    /// A connector's view, while it exists. For tests.
    pub fn connector_panel(&self, provider: ConnectorProvider) -> Option<Entity<ConnectorPanel>> {
        self.connectors.get(&provider).cloned()
    }

    /// The GitHub view, while it exists. For tests.
    pub fn github_panel(&self) -> Option<Entity<GithubPanel>> {
        self.github.clone()
    }

    /// Escape: back one view, or close from the list.
    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if any_composing(&[&self.name], cx) {
            return;
        }
        if self.view == IntegrationsView::Github {
            let Some(panel) = self.github.clone() else {
                return self.show_list(window, cx);
            };
            if !panel.update(cx, |panel, cx| panel.back(window, cx)) {
                self.view = IntegrationsView::List;
                window.focus(&self.github_focus, cx);
                cx.notify();
            }
            return;
        }
        if self.view == IntegrationsView::Apps {
            let Some(panel) = self.apps.clone() else {
                return self.show_list(window, cx);
            };
            if !panel.update(cx, |panel, cx| panel.back(window, cx)) {
                self.leave_apps(window, cx);
            }
            return;
        }
        if let IntegrationsView::Connector(provider) = self.view {
            let Some(panel) = self.connectors.get(&provider).cloned() else {
                return self.show_list(window, cx);
            };
            if !panel.update(cx, |panel, cx| panel.back(window, cx)) {
                self.leave_connector(provider, window, cx);
            }
            return;
        }
        match self.view {
            IntegrationsView::List => self.finish(window, cx),
            IntegrationsView::Adding | IntegrationsView::Details(_) => self.show_list(window, cx),
            IntegrationsView::Address { back } => self.leave_address(back, window, cx),
            IntegrationsView::ConfirmNewAddress(id) | IntegrationsView::ConfirmDelete(id) => {
                self.open_details(id, window, cx)
            }
            IntegrationsView::Github | IntegrationsView::Connector(_) | IntegrationsView::Apps => {}
        }
    }

    /// Closes the dialog: nothing of it may outlive it.
    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.forget_address(cx);
        let mut kinds = vec![ActionKind::LoadWorkspaceWebhooks(self.workspace)];
        if let Some(channel) = self.channel {
            kinds.push(ActionKind::CreateWebhook(channel));
        }
        if let Some(panel) = &self.github {
            kinds.extend(panel.read(cx).kinds());
        }
        for panel in self.connectors.values() {
            kinds.extend(panel.read(cx).kinds());
            panel.update(cx, |panel, cx| panel.forget_secret(cx));
        }
        if let Some(panel) = &self.apps {
            kinds.extend(panel.read(cx).kinds());
            panel.update(cx, |panel, cx| panel.forget_token(cx));
        }
        for id in self.manage_focus.keys() {
            kinds.push(ActionKind::NewWebhookAddress(*id));
            kinds.push(ActionKind::DeleteWebhook(*id));
            kinds.push(ActionKind::LoadWebhookDeliveries(*id));
        }
        self.model.update(cx, |model, cx| {
            for kind in kinds {
                model.dispatch(Input::DismissAction(kind), cx);
            }
        });
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }

    // ----- Acting -----

    /// The channels a new webhook can go in: the person's open channels in this workspace, by
    /// name.
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

    fn create(&mut self, cx: &mut Context<Self>) {
        let Some(conversation) = self.channel else {
            return;
        };
        let name = self.name.read(cx).text().trim().to_owned();
        let kind = ActionKind::CreateWebhook(conversation);
        if name.is_empty() || self.action(kind, cx) == ActionState::InFlight {
            return;
        }
        self.waiting = Some((kind, false));
        self.dispatch(Input::CreateWebhook { conversation, name }, cx);
    }

    fn new_address(&mut self, id: WebhookId, cx: &mut Context<Self>) {
        let kind = ActionKind::NewWebhookAddress(id);
        if self.action(kind, cx) == ActionState::InFlight {
            return;
        }
        self.waiting = Some((kind, false));
        self.dispatch(Input::NewWebhookAddress(id), cx);
    }

    fn delete(&mut self, id: WebhookId, cx: &mut Context<Self>) {
        let kind = ActionKind::DeleteWebhook(id);
        if self.action(kind, cx) == ActionState::InFlight {
            return;
        }
        self.waiting = Some((kind, false));
        self.dispatch(Input::DeleteWebhook(id), cx);
    }

    fn copy_address(&mut self, cx: &mut Context<Self>) {
        let url = self
            .model
            .read(cx)
            .core()
            .created_webhook()
            .map(|created| created.url.as_str().to_owned());
        if let Some(url) = url {
            cx.write_to_clipboard(ClipboardItem::new_string(url));
            self.copied = true;
            cx.notify();
        }
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
            .debug_selector(|| "integrations-error".into())
            .text_size(theme.type_scale.small)
            .text_color(theme.colors.danger)
            .child(message)
            .into_any_element()
    }

    /// A service's tile in the list: its mark and name, where it stands, failures if any, and the
    /// one way in. `key` names its parts for tests (`{key}-card`, `{key}-card-status`,
    /// `{key}-open-panel`).
    #[allow(clippy::too_many_arguments)]
    fn tile(
        &self,
        theme: &Theme,
        key: &'static str,
        name: &str,
        mark: (&'static str, u32),
        state: TileState,
        focus: &FocusHandle,
        on_open: impl Fn(&mut Window, &mut gpui::App) + 'static,
    ) -> AnyElement {
        let colors = theme.colors;
        div()
            .debug_selector(move || format!("{key}-card"))
            .flex()
            .items_center()
            .gap_3()
            .min_w_0()
            .px_3()
            .py_2p5()
            .rounded(theme.radii.panel)
            .border_1()
            .border_color(colors.border)
            .bg(colors.surface)
            .child(ui::service_mark(theme, mark.0, mark.1, 34.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme.type_scale.body)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(colors.text)
                                    .child(name.to_owned()),
                            )
                            .when(state.failed > 0, |row| {
                                row.child(ui::chip(
                                    theme,
                                    format!("{} failed", state.failed),
                                    ui::Tone::Problem,
                                ))
                            }),
                    )
                    .child(ui::status_line(
                        theme,
                        format!("{key}-card-status"),
                        state.status,
                        state.tone,
                    )),
            )
            .child(form::button(
                theme,
                format!("{key}-open-panel"),
                state.action,
                // Filled only when it needs the admin; otherwise a quiet way in.
                if state.tone == ui::Tone::Attention {
                    ButtonKind::Primary
                } else {
                    ButtonKind::Secondary
                },
                true,
                focus,
                on_open,
            ))
            .into_any_element()
    }

    fn github_tile(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let status = self.model.read(cx).core().github(self.workspace).cloned();
        let state = match &status {
            None => TileState::quiet("Repositories' activity", "Connect"),
            Some(s) if !s.available => TileState::quiet("Needs the credentials key", "Details"),
            Some(s) => match &s.connection {
                None => TileState::quiet("Not connected", "Connect"),
                Some(_) if s.installations.is_empty() => TileState {
                    status: "Install it on GitHub to finish".into(),
                    tone: ui::Tone::Attention,
                    failed: 0,
                    action: "Finish setup",
                },
                Some(c) => TileState::live(
                    format!("{} · {}", c.owner, channels_word(s.subscriptions.len())),
                    failed_count(s.deliveries.iter().map(|d| d.state)),
                ),
            },
        };
        let open_to = cx.entity().downgrade();
        self.tile(
            theme,
            "github",
            "GitHub",
            ui::marks::GITHUB,
            state,
            &self.github_focus,
            move |window, cx| {
                open_to
                    .update(cx, |dialog, cx| dialog.open_github(window, cx))
                    .ok();
            },
        )
    }

    fn connector_tile(
        &self,
        provider: ConnectorProvider,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let status = self
            .model
            .read(cx)
            .core()
            .connector(self.workspace, provider)
            .cloned();
        let what = match provider {
            ConnectorProvider::Linear => "Issues and project updates",
            ConnectorProvider::Jira => "Issues and comments",
            ConnectorProvider::Sentry => "Errors and alerts",
            ConnectorProvider::PagerDuty => "Incidents",
            ConnectorProvider::Unknown => "Activity",
        };
        let state = match &status {
            None => TileState::quiet(what, "Connect"),
            Some(s) if !s.available => TileState::quiet("Needs the credentials key", "Details"),
            Some(s) => match &s.connection {
                None => TileState::quiet(what, "Connect"),
                Some(c) if !c.secret_set => TileState {
                    status: "Needs its webhook".into(),
                    tone: ui::Tone::Attention,
                    failed: 0,
                    action: "Finish setup",
                },
                Some(_) => TileState::live(
                    channels_word(s.subscriptions.len()),
                    failed_count(s.deliveries.iter().map(|d| d.state)),
                ),
            },
        };
        let open_to = cx.entity().downgrade();
        let key: &'static str = match provider {
            ConnectorProvider::Linear => "linear",
            ConnectorProvider::Jira => "jira",
            ConnectorProvider::Sentry => "sentry",
            ConnectorProvider::PagerDuty => "pagerduty",
            ConnectorProvider::Unknown => "unknown",
        };
        let focus = self
            .connector_focus
            .get(&provider)
            .cloned()
            .unwrap_or_else(|| self.github_focus.clone());
        self.tile(
            theme,
            key,
            provider.label(),
            ui::marks::of(provider),
            state,
            &focus,
            move |window, cx| {
                open_to
                    .update(cx, |dialog, cx| dialog.open_connector(provider, window, cx))
                    .ok();
            },
        )
    }

    fn apps_tile(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let apps = self
            .model
            .read(cx)
            .core()
            .apps(self.workspace)
            .map(<[lazy_protocol::App]>::to_vec);
        let state = match apps.as_deref() {
            None | Some([]) => {
                TileState::quiet("Bots with a token, commands, and events", "Make an app")
            }
            Some(apps) => {
                let commands: usize = apps.iter().map(|a| a.commands.len()).sum();
                let mut status = match apps.len() {
                    1 => "1 app".to_owned(),
                    n => format!("{n} apps"),
                };
                if commands > 0 {
                    status.push_str(&format!(
                        " · {commands} slash command{}",
                        if commands == 1 { "" } else { "s" }
                    ));
                }
                TileState::live(status, 0)
            }
        };
        let open_to = cx.entity().downgrade();
        self.tile(
            theme,
            "apps",
            "Apps and bots",
            ui::marks::APPS,
            state,
            &self.apps_focus,
            move |window, cx| {
                open_to
                    .update(cx, |dialog, cx| dialog.open_apps(window, cx))
                    .ok();
            },
        )
    }

    fn list_view(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let kind = ActionKind::LoadWorkspaceWebhooks(self.workspace);
        let (entries, loading, load_error) = {
            let core = self.model.read(cx).core();
            (
                core.workspace_webhooks(self.workspace).to_vec(),
                core.action(kind) == &ActionState::InFlight,
                match core.action(kind) {
                    ActionState::Failed(message) => Some(message.clone()),
                    _ => None,
                },
            )
        };
        let can_add = !self.channels(cx).is_empty();
        let now = Zoned::now();
        let mut list = div()
            .id("workspace-webhook-list")
            .debug_selector(|| "workspace-webhook-list".into())
            .max_h(px(360.))
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            .flex()
            .flex_col()
            .gap_1();
        for (index, entry) in entries.iter().enumerate() {
            let id = entry.webhook.id;
            let focus = Self::handle_for(&mut self.manage_focus, id, cx);
            let creator = self
                .model
                .read(cx)
                .core()
                .display_name(entry.webhook.created_by)
                .to_owned();
            let added = ago_label(&to_zoned(entry.webhook.created_at, &self.zone), &now);
            let (health, health_color) = health_line(entry, &self.zone, &now, theme);
            let open_to = this.clone();
            list = list.child(
                div()
                    .debug_selector(|| "workspace-webhook-row".into())
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
                                    .child(entry.webhook.name.to_string()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.text_muted)
                                    .child(format!(
                                        "#{} · Added by {creator} · {added}",
                                        entry.channel_name.as_str()
                                    )),
                            )
                            .child(
                                div()
                                    .debug_selector(|| "workspace-webhook-health".into())
                                    .truncate()
                                    .text_size(theme.type_scale.small)
                                    .text_color(health_color)
                                    .child(health),
                            ),
                    )
                    .child(form::button(
                        theme,
                        format!("workspace-webhook-manage-{index}"),
                        "Manage",
                        ButtonKind::Secondary,
                        true,
                        &focus,
                        move |window, cx| {
                            open_to
                                .update(cx, |dialog, cx| dialog.open_details(id, window, cx))
                                .ok();
                        },
                    )),
            );
        }
        let add_to = this.clone();
        let retry_to = this;
        let workspace = self.workspace;
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(ui::section(
                theme,
                "Services",
                Some("Post what happens in your team's tools into the channels you choose.".into()),
            ))
            .child(
                div()
                    .debug_selector(|| "integrations-services".into())
                    .grid()
                    .grid_cols(2)
                    .gap_2()
                    .child(self.github_tile(theme, cx))
                    .children(
                        ConnectorProvider::ALL
                            .into_iter()
                            .map(|provider| self.connector_tile(provider, theme, cx)),
                    ),
            )
            .child(ui::divider(theme))
            .child(ui::section(
                theme,
                "Build your own",
                Some(
                    "Your own programs: an app with a token, slash commands and events, or a \
                     webhook address that posts into one channel."
                        .into(),
                ),
            ))
            .child(self.apps_tile(theme, cx))
            .child(
                div()
                    .pt_1()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(ui::service_mark(
                        theme,
                        ui::marks::WEBHOOK.0,
                        ui::marks::WEBHOOK.1,
                        34.,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_size(theme.type_scale.body)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(colors.text)
                                    .child("Incoming webhooks"),
                            )
                            .child(self.note(
                                theme,
                                "integrations-intro",
                                "A secret address that posts into one channel as an app. Tools \
                                 with a Slack option can use it too.",
                            )),
                    ),
            )
            .when(loading && entries.is_empty(), |view| {
                view.child(self.note(theme, "integrations-loading", "Loading integrations…"))
            })
            .children(load_error.clone().map(|message| {
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(self.error_line(theme, states::integrations_failed(&message))),
                    )
                    .child(form::button(
                        theme,
                        "integrations-retry",
                        "Try again",
                        ButtonKind::Secondary,
                        true,
                        &self.retry_focus,
                        move |_, cx| {
                            retry_to
                                .update(cx, |dialog, cx| {
                                    dialog.dispatch(Input::LoadWorkspaceWebhooks(workspace), cx)
                                })
                                .ok();
                        },
                    ))
            }))
            .when(
                !loading && load_error.is_none() && entries.is_empty(),
                |view| {
                    view.child(self.note(
                        theme,
                        "integrations-empty",
                        "No incoming webhooks yet. Add one to let a tool post into a channel.",
                    ))
                },
            )
            .child(list)
            .when(!can_add, |view| {
                view.child(self.note(
                    theme,
                    "integrations-no-channel",
                    "Join a channel that isn't archived to add a webhook to it.",
                ))
            })
            .child(div().flex().child(form::button(
                theme,
                "workspace-webhook-add",
                "Add incoming webhook",
                ButtonKind::Primary,
                can_add,
                &self.add_focus,
                move |window, cx| {
                    add_to
                        .update(cx, |dialog, cx| dialog.start_adding(window, cx))
                        .ok();
                },
            )))
            .into_any_element()
    }

    fn adding_view(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let channels = self.channels(cx);
        let (state, full) = match self.channel {
            Some(channel) => {
                let core = self.model.read(cx).core();
                let in_channel = core
                    .workspace_webhooks(self.workspace)
                    .iter()
                    .filter(|entry| entry.webhook.conversation_id == channel)
                    .count();
                (
                    core.action(ActionKind::CreateWebhook(channel)).clone(),
                    in_channel >= MAX_WEBHOOKS_PER_CONVERSATION,
                )
            }
            None => (ActionState::Idle, false),
        };
        let working = state == ActionState::InFlight;
        let error = match &state {
            ActionState::Failed(message) => Some(message.clone()),
            _ => None,
        };
        let ready = self.channel.is_some()
            && !full
            && !working
            && !self.name.read(cx).text().trim().is_empty();
        let mut choices = div()
            .id("webhook-channel-choices")
            .debug_selector(|| "webhook-channel-choices".into())
            .max_h(px(140.))
            .overflow_y_scroll()
            .flex()
            .flex_wrap()
            .gap_1();
        for (index, (id, name)) in channels.iter().enumerate() {
            let id = *id;
            let focus = Self::handle_for(&mut self.channel_focus, id, cx);
            let chosen = self.channel == Some(id);
            let choose_to = this.clone();
            choices = choices.child(form::button(
                theme,
                format!("webhook-channel-{index}"),
                format!("#{name}"),
                if chosen {
                    ButtonKind::Primary
                } else {
                    ButtonKind::Secondary
                },
                !working,
                &focus,
                move |_, cx| {
                    choose_to
                        .update(cx, |dialog, cx| {
                            if let Some(old) = dialog.channel {
                                dialog.dispatch(
                                    Input::DismissAction(ActionKind::CreateWebhook(old)),
                                    cx,
                                );
                            }
                            dialog.channel = Some(id);
                            cx.notify();
                        })
                        .ok();
                },
            ));
        }
        let cancel_to = this.clone();
        let create_to = this;
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(theme.type_scale.small)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.text_muted)
                            .child("Channel"),
                    )
                    .child(choices)
                    .when(full, |section| {
                        section.child(self.note(
                            theme,
                            "integrations-full",
                            format!(
                                "This channel has {MAX_WEBHOOKS_PER_CONVERSATION} incoming webhooks, \
                                 the most it can have. Delete one to add another."
                            ),
                        ))
                    }),
            )
            .child(form::labeled_field(
                theme,
                "Name of the app",
                &self.name,
                Some("It is shown as the author of what it posts.".into()),
                error.map(Into::into),
                window,
                cx,
            ))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "workspace-webhook-cancel",
                        "Cancel",
                        ButtonKind::Secondary,
                        true,
                        &self.cancel_focus,
                        move |window, cx| {
                            cancel_to
                                .update(cx, |dialog, cx| dialog.show_list(window, cx))
                                .ok();
                        },
                    ))
                    .child(form::button(
                        theme,
                        "workspace-webhook-create",
                        if working {
                            "Creating…"
                        } else {
                            "Create webhook"
                        },
                        ButtonKind::Primary,
                        ready,
                        &self.create_focus,
                        move |_, cx| {
                            create_to.update(cx, |dialog, cx| dialog.create(cx)).ok();
                        },
                    )),
            )
            .into_any_element()
    }

    fn address_view(
        &self,
        theme: &Theme,
        back: Option<WebhookId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let copy_to = this.clone();
        let done_to = this;
        let name = self
            .model
            .read(cx)
            .core()
            .created_webhook()
            .map(|created| created.webhook.name.to_string())
            .unwrap_or_default();
        let focused = self.address.read(cx).is_focused(window);
        let lead = if back.is_some() {
            format!(
                "{name} has a new address. The old one has stopped working: update anything \
                 that posts to it. Copy this one now: it will not be shown again."
            )
        } else {
            format!(
                "This address lets anyone post as {name}. Copy it now: it will not be shown again."
            )
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .debug_selector(|| "webhook-note".into())
                    .text_size(theme.type_scale.body)
                    .text_color(colors.text)
                    .child(lead),
            )
            .child(
                div()
                    .debug_selector(|| "webhook-address".into())
                    .px_3()
                    .py_2()
                    .rounded(theme.radii.medium)
                    .bg(colors.surface_raised)
                    .border_1()
                    .border_color(if focused {
                        colors.focus_ring
                    } else {
                        colors.border_strong
                    })
                    .font_family(fonts::MONO_FAMILY)
                    .child(self.address.clone()),
            )
            .child(self.note(
                theme,
                "webhook-usage",
                "To post, send a POST to it with a JSON body like {\"text\": \"Deploy finished\"}. \
                 Tools with a Slack option can use this address there too. Add an \
                 Idempotency-Key header so a retry posts once.",
            ))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "webhook-copy",
                        if self.copied {
                            "Copied"
                        } else {
                            "Copy address"
                        },
                        ButtonKind::Secondary,
                        true,
                        &self.copy_focus,
                        move |_, cx| {
                            copy_to
                                .update(cx, |dialog, cx| dialog.copy_address(cx))
                                .ok();
                        },
                    ))
                    .child(form::button(
                        theme,
                        "webhook-done",
                        "Done",
                        ButtonKind::Primary,
                        true,
                        &self.done_focus,
                        move |window, cx| {
                            done_to
                                .update(cx, |dialog, cx| dialog.leave_address(back, window, cx))
                                .ok();
                        },
                    )),
            )
            .into_any_element()
    }

    fn details_view(&self, id: WebhookId, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let Some(entry) = self.entry(id, cx) else {
            return self.note(theme, "integrations-loading", "Loading integrations…");
        };
        let this = cx.entity().downgrade();
        let kind = ActionKind::LoadWebhookDeliveries(id);
        let (log, loading, load_error, creator) = {
            let core = self.model.read(cx).core();
            (
                core.webhook_deliveries(id).map(<[WebhookDelivery]>::to_vec),
                core.action(kind) == &ActionState::InFlight,
                match core.action(kind) {
                    ActionState::Failed(message) => Some(message.clone()),
                    _ => None,
                },
                core.display_name(entry.webhook.created_by).to_owned(),
            )
        };
        let now = Zoned::now();
        let added = ago_label(&to_zoned(entry.webhook.created_at, &self.zone), &now);
        let mut rows = div()
            .id("webhook-delivery-log")
            .debug_selector(|| "webhook-delivery-log".into())
            .max_h(px(260.))
            .overflow_y_scroll()
            .track_scroll(&self.log_scroll)
            .flex()
            .flex_col();
        for delivery in log.iter().flatten() {
            let (label, explanation, color) = outcome_words(delivery.outcome, theme);
            let when = ago_label(&to_zoned(delivery.received_at, &self.zone), &now);
            let mut facts = vec![when];
            if delivery.body_bytes > 0 {
                facts.push(size_label(delivery.body_bytes));
            }
            if delivery.idempotency_key {
                facts.push("with Idempotency-Key".to_owned());
            }
            rows = rows.child(
                div()
                    .debug_selector(|| "webhook-delivery".into())
                    .flex()
                    .items_start()
                    .gap_3()
                    .py_1p5()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .w(px(150.))
                            .flex_none()
                            .text_size(theme.type_scale.small)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(color)
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.text)
                                    .child(facts.join(" · ")),
                            )
                            .children(explanation.map(|text| {
                                div()
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.text_muted)
                                    .child(text)
                            })),
                    ),
            );
        }
        let back_to = this.clone();
        let refresh_to = this.clone();
        let renew_to = this.clone();
        let delete_to = this;
        let log_empty = log.as_ref().is_some_and(Vec::is_empty);
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .debug_selector(|| "webhook-details-name".into())
                                    .truncate()
                                    .text_size(theme.type_scale.title)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(colors.text)
                                    .child(entry.webhook.name.to_string()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.text_muted)
                                    .child(format!(
                                        "Posts into #{} · Added by {creator} · {added}",
                                        entry.channel_name.as_str()
                                    )),
                            ),
                    )
                    .child(form::button(
                        theme,
                        "webhook-details-back",
                        "All integrations",
                        ButtonKind::Secondary,
                        true,
                        &self.back_focus,
                        move |window, cx| {
                            back_to
                                .update(cx, |dialog, cx| dialog.show_list(window, cx))
                                .ok();
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_size(theme.type_scale.small)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.text_muted)
                            .child(format!(
                                "Recent requests (the last {MAX_WEBHOOK_DELIVERIES_KEPT} are kept)"
                            )),
                    )
                    .child(form::button(
                        theme,
                        "webhook-details-refresh",
                        if loading { "Refreshing…" } else { "Refresh" },
                        ButtonKind::Secondary,
                        !loading,
                        &self.refresh_focus,
                        move |_, cx| {
                            refresh_to
                                .update(cx, |dialog, cx| {
                                    dialog.dispatch(Input::LoadWebhookDeliveries(id), cx)
                                })
                                .ok();
                        },
                    )),
            )
            .children(load_error.map(|message| {
                self.error_line(theme, format!("The requests didn't load. {message}"))
            }))
            .when(log.is_none() && loading, |view| {
                view.child(self.note(theme, "webhook-log-loading", "Loading requests…"))
            })
            .when(log_empty, |view| {
                view.child(self.note(
                    theme,
                    "webhook-log-empty",
                    "No requests yet. Each request to this webhook's address will show here, \
                     with what became of it.",
                ))
            })
            .child(rows)
            .child(self.note(
                theme,
                "webhook-log-retry",
                "Lazy doesn't keep what was sent, so it can't send a failed request again. The \
                 sender can retry; with the same Idempotency-Key, a retry posts once.",
            ))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "webhook-new-address",
                        "New address…",
                        ButtonKind::Secondary,
                        true,
                        &self.new_address_focus,
                        move |window, cx| {
                            renew_to
                                .update(cx, |dialog, cx| {
                                    dialog.show_confirm_new_address(id, window, cx)
                                })
                                .ok();
                        },
                    ))
                    .child(form::button(
                        theme,
                        "webhook-delete",
                        "Delete…",
                        ButtonKind::Danger,
                        true,
                        &self.delete_focus,
                        move |window, cx| {
                            delete_to
                                .update(cx, |dialog, cx| dialog.show_confirm_delete(id, window, cx))
                                .ok();
                        },
                    )),
            )
            .into_any_element()
    }

    fn confirm_view(
        &self,
        id: WebhookId,
        deleting: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let cancel_to = this.clone();
        let go_to = this;
        let name = self
            .entry(id, cx)
            .map(|entry| entry.webhook.name.to_string())
            .unwrap_or_else(|| "this webhook".to_owned());
        let kind = if deleting {
            ActionKind::DeleteWebhook(id)
        } else {
            ActionKind::NewWebhookAddress(id)
        };
        let state = self.action(kind, cx);
        let working = state == ActionState::InFlight;
        let error = match &state {
            ActionState::Failed(message) => Some(message.clone()),
            _ => None,
        };
        let (question, button, busy, button_kind) = if deleting {
            (
                format!(
                    "Delete {name}? Anything still posting to its address stops working, and the \
                     app leaves its channel. What it already posted stays."
                ),
                "Delete webhook",
                "Deleting…",
                ButtonKind::Danger,
            )
        } else {
            (
                format!(
                    "Give {name} a new address? The current address stops working at once, so \
                     anything that posts to it must be given the new one. Use this if the address \
                     may have been seen by someone who shouldn't have it."
                ),
                "Make a new address",
                "Making…",
                ButtonKind::Primary,
            )
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .debug_selector(|| "webhook-confirm-question".into())
                    .text_size(theme.type_scale.body)
                    .text_color(colors.text)
                    .child(question),
            )
            .children(error.map(|message| self.error_line(theme, message)))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "webhook-confirm-cancel",
                        "Cancel",
                        ButtonKind::Secondary,
                        !working,
                        &self.confirm_cancel_focus,
                        move |window, cx| {
                            cancel_to
                                .update(cx, |dialog, cx| dialog.open_details(id, window, cx))
                                .ok();
                        },
                    ))
                    .child(form::button(
                        theme,
                        "webhook-confirm",
                        if working { busy } else { button },
                        button_kind,
                        !working,
                        &self.confirm_focus,
                        move |_, cx| {
                            go_to
                                .update(cx, |dialog, cx| {
                                    if deleting {
                                        dialog.delete(id, cx);
                                    } else {
                                        dialog.new_address(id, cx);
                                    }
                                })
                                .ok();
                        },
                    )),
            )
            .into_any_element()
    }
}

/// How a webhook is doing, for its row in the list, and the color to say it in.
fn health_line(
    entry: &WorkspaceWebhook,
    zone: &TimeZone,
    now: &Zoned,
    theme: &Theme,
) -> (String, Hsla) {
    let colors = theme.colors;
    let Some(last) = &entry.last_delivery else {
        return ("No requests yet".to_owned(), colors.text_muted);
    };
    let when = ago_label(&to_zoned(last.received_at, zone), now);
    let (label, _, color) = outcome_words(last.outcome, theme);
    let mut text = format!("Last request {when}: {label}");
    if entry.recent_failures > 0 {
        let n = entry.recent_failures;
        text.push_str(&format!(
            " · {n} {} in the last day",
            if n == 1 { "failure" } else { "failures" }
        ));
        return (text, colors.danger);
    }
    (text, color)
}

/// What a delivery's outcome is called, what it means when that needs saying, and its color.
pub fn outcome_words(
    outcome: WebhookDeliveryOutcome,
    theme: &Theme,
) -> (&'static str, Option<&'static str>, Hsla) {
    let colors = theme.colors;
    match outcome {
        WebhookDeliveryOutcome::Posted => ("Posted", None, colors.success),
        WebhookDeliveryOutcome::Duplicate => (
            "Already posted",
            Some("The Idempotency-Key was used before, so nothing new was posted."),
            colors.text_muted,
        ),
        WebhookDeliveryOutcome::RateLimited => (
            "Too fast",
            Some("Refused until the webhook slows down (counted once per run of refusals)."),
            colors.warning,
        ),
        WebhookDeliveryOutcome::TooLarge => {
            ("Too large", Some("The body was over 64 KB."), colors.danger)
        }
        WebhookDeliveryOutcome::Invalid => (
            "Couldn't be read",
            Some(
                "Not JSON like {\"text\": \"…\"} or a Slack webhook body, nothing to post, text \
                 too long for a message, or an Idempotency-Key used again with different text.",
            ),
            colors.danger,
        ),
        WebhookDeliveryOutcome::Archived => (
            "Channel archived",
            Some("Unarchive the channel to let it post again."),
            colors.warning,
        ),
        WebhookDeliveryOutcome::Failed => (
            "Server error",
            Some("Lazy failed while posting. The sender can try again."),
            colors.danger,
        ),
        WebhookDeliveryOutcome::Unknown => (
            "Unknown",
            Some("A newer server recorded something this version can't name."),
            colors.text_muted,
        ),
    }
}

/// A body's size for people: bytes up to a kilobyte, then kilobytes.
/// What a service's tile says.
struct TileState {
    status: String,
    tone: ui::Tone,
    failed: usize,
    action: &'static str,
}

impl TileState {
    fn quiet(status: impl Into<String>, action: &'static str) -> Self {
        Self {
            status: status.into(),
            tone: ui::Tone::Quiet,
            failed: 0,
            action,
        }
    }

    fn live(status: String, failed: usize) -> Self {
        Self {
            status,
            // Failures have their own badge; the line still says what is set up.
            tone: ui::Tone::Good,
            failed,
            action: "Manage",
        }
    }
}

fn channels_word(n: usize) -> String {
    match n {
        0 => "No channels yet".to_owned(),
        1 => "1 channel".to_owned(),
        n => format!("{n} channels"),
    }
}

fn failed_count(states: impl Iterator<Item = lazy_protocol::GithubDeliveryState>) -> usize {
    states
        .filter(|s| {
            matches!(
                s,
                lazy_protocol::GithubDeliveryState::Failed
                    | lazy_protocol::GithubDeliveryState::Unknown
            )
        })
        .count()
}

fn size_label(bytes: u32) -> String {
    if bytes < 1024 {
        format!("{bytes} bytes")
    } else {
        format!("{:.1} KB", f64::from(bytes) / 1024.0)
    }
}

impl Render for IntegrationsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let close_to = cx.entity().downgrade();
        let body = match self.view {
            IntegrationsView::List => self.list_view(&theme, cx),
            IntegrationsView::Adding => self.adding_view(&theme, window, cx),
            IntegrationsView::Details(id) => self.details_view(id, &theme, cx),
            IntegrationsView::Address { back } => self.address_view(&theme, back, window, cx),
            IntegrationsView::ConfirmNewAddress(id) => self.confirm_view(id, false, &theme, cx),
            IntegrationsView::ConfirmDelete(id) => self.confirm_view(id, true, &theme, cx),
            IntegrationsView::Github => match &self.github {
                Some(panel) => panel.clone().into_any_element(),
                None => self.list_view(&theme, cx),
            },
            IntegrationsView::Connector(provider) => match self.connectors.get(&provider) {
                Some(panel) => panel.clone().into_any_element(),
                None => self.list_view(&theme, cx),
            },
            IntegrationsView::Apps => match &self.apps {
                Some(panel) => panel.clone().into_any_element(),
                None => self.list_view(&theme, cx),
            },
        };
        let body = div()
            .debug_selector(|| "workspace-integrations".into())
            .child(body)
            .into_any_element();
        dialog_overlay(
            &theme,
            "Integrations",
            720.,
            &self.trap,
            body,
            "Close",
            move |window, cx| {
                close_to
                    .update(cx, |dialog, cx| dialog.finish(window, cx))
                    .ok();
            },
        )
        .on_action(cx.listener(|this, _: &DialogDismiss, window, cx| this.back(window, cx)))
        .on_action(cx.listener(|this, _: &DialogNext, window, cx| this.trap.tab(false, window, cx)))
        .on_action(cx.listener(|this, _: &DialogPrev, window, cx| this.trap.tab(true, window, cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_as_bytes_then_kilobytes() {
        assert_eq!(size_label(18), "18 bytes");
        assert_eq!(size_label(2048), "2.0 KB");
        assert_eq!(size_label(65_537), "64.0 KB");
    }
}
