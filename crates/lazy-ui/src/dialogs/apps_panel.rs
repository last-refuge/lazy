//! Apps, inside the Integrations dialog (admins and owners, D35): bots with tokens for Lazy's bot
//! API. Making one, its token (shown once), what it may do, which channels its bot is in, a new
//! token, and deleting it.
//!
//! # Views
//!
//! - **List**: the apps, each with Manage, and Make an app.
//! - **Making**: the name and what it may do, then Make.
//! - **Details**: the token (shown once right after it is made, with Copy; afterwards only its
//!   last four characters and New token…), what it may do (toggled at once), its channels with
//!   Remove and the channels it could be added to, and Delete….
//! - **Confirm** a new token or deleting.
//!
//! # Keyboard
//!
//! One tab stop per control, in the order drawn; Tab and Shift+Tab in the name field move on
//! through the dialog; Enter in it makes the app. Escape steps back one view; from the list the
//! dialog returns to its own list.

use std::collections::HashMap;

use gpui::{
    AnyElement, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    IntoElement, ParentElement, Render, Styled, Subscription, Window, div, prelude::*, px,
};
use lazy_client_core::{ActionKind, ActionState, Input};
use lazy_domain::app::AppScope;
use lazy_domain::{AppId, ConversationId, ConversationKind, WorkspaceId};
use lazy_protocol::App;

use super::single_line_field;
use crate::app_model::{AppEvent, AppModel};
use crate::fonts;
use crate::form::{self, ButtonKind};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::Theme;

/// What the panel asks of the dialog around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppsPanelEvent {
    Tab { backwards: bool },
    Leave,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppsView {
    List,
    Making,
    Details(AppId),
    ConfirmNewToken(AppId),
    ConfirmDelete(AppId),
}

pub struct AppsPanel {
    model: Entity<AppModel>,
    workspace: WorkspaceId,
    view: AppsView,
    name: Entity<TextInput>,
    scopes: Vec<AppScope>,
    copied: bool,
    /// A request whose end moves the view, and whether it has been seen starting.
    waiting: Option<(ActionKind, bool)>,
    back_focus: FocusHandle,
    make_focus: FocusHandle,
    cancel_focus: FocusHandle,
    confirm_focus: FocusHandle,
    copy_focus: FocusHandle,
    new_token_focus: FocusHandle,
    delete_focus: FocusHandle,
    manage_focus: HashMap<AppId, FocusHandle>,
    scope_focus: HashMap<AppScope, FocusHandle>,
    channel_focus: HashMap<ConversationId, FocusHandle>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<AppsPanelEvent> for AppsPanel {}

impl AppsPanel {
    pub fn new(
        model: Entity<AppModel>,
        workspace: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = single_line_field("e.g. Deploy bot", "Name of the app", 64, cx);
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &name,
            window,
            |this, _input, event: &TextInputEvent, _window, cx| match event {
                TextInputEvent::Submit => this.make(cx),
                TextInputEvent::Tab { backwards } => cx.emit(AppsPanelEvent::Tab {
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
            model.dispatch(Input::DismissAppToken, cx);
            model.dispatch(Input::LoadApps(workspace), cx);
        });
        Self {
            model,
            workspace,
            view: AppsView::List,
            name,
            scopes: AppScope::DEFAULT.to_vec(),
            copied: false,
            waiting: None,
            back_focus: cx.focus_handle().tab_stop(true),
            make_focus: cx.focus_handle().tab_stop(true),
            cancel_focus: cx.focus_handle().tab_stop(true),
            confirm_focus: cx.focus_handle().tab_stop(true),
            copy_focus: cx.focus_handle().tab_stop(true),
            new_token_focus: cx.focus_handle().tab_stop(true),
            delete_focus: cx.focus_handle().tab_stop(true),
            manage_focus: HashMap::new(),
            scope_focus: HashMap::new(),
            channel_focus: HashMap::new(),
            _subscriptions: subscriptions,
        }
    }

    // ----- For the dialog and tests -----

    pub fn view(&self) -> AppsView {
        self.view
    }

    pub fn first_focus(&self) -> FocusHandle {
        self.back_focus.clone()
    }

    pub fn name_input(&self) -> &Entity<TextInput> {
        &self.name
    }

    pub fn composing(&self, cx: &gpui::App) -> bool {
        self.name.read(cx).is_composing()
    }

    /// The action kinds the panel may have left failed, to clear when the dialog closes.
    pub fn kinds(&self) -> Vec<ActionKind> {
        let mut kinds = vec![
            ActionKind::LoadApps(self.workspace),
            ActionKind::CreateApp(self.workspace),
        ];
        for app in self.manage_focus.keys() {
            kinds.push(ActionKind::NewAppToken(*app));
            kinds.push(ActionKind::UpdateAppScopes(*app));
            kinds.push(ActionKind::DeleteApp(*app));
            for channel in self.channel_focus.keys() {
                kinds.push(ActionKind::AppChannel(*app, *channel));
            }
        }
        kinds
    }

    /// Escape: back one view. Returns whether it did anything (from the list it does not).
    pub fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.composing(cx) {
            return true;
        }
        match self.view {
            AppsView::List => false,
            AppsView::Making | AppsView::Details(_) => {
                self.show_list(window, cx);
                true
            }
            AppsView::ConfirmNewToken(app) | AppsView::ConfirmDelete(app) => {
                self.show_details(app, window, cx);
                true
            }
        }
    }

    /// Forgets a token this panel showed (the dialog is closing).
    pub fn forget_token(&mut self, cx: &mut Context<Self>) {
        self.dispatch(Input::DismissAppToken, cx);
    }

    // ----- Following the core -----

    fn apps(&self, cx: &gpui::App) -> Option<Vec<App>> {
        self.model
            .read(cx)
            .core()
            .apps(self.workspace)
            .map(<[App]>::to_vec)
    }

    fn app(&self, id: AppId, cx: &gpui::App) -> Option<App> {
        self.apps(cx)?.into_iter().find(|a| a.id == id)
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
                        ActionKind::CreateApp(_) => {
                            let made = self.model.read(cx).core().app_token_for();
                            if let Some(app) = made {
                                self.show_details(app, window, cx);
                            }
                        }
                        ActionKind::NewAppToken(app) => self.show_details(app, window, cx),
                        ActionKind::DeleteApp(_) => self.show_list(window, cx),
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

    fn show_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dispatch(Input::DismissAppToken, cx);
        self.view = AppsView::List;
        window.focus(&self.back_focus, cx);
        cx.notify();
    }

    pub fn show_details(&mut self, app: AppId, window: &mut Window, cx: &mut Context<Self>) {
        self.copied = false;
        self.view = AppsView::Details(app);
        window.focus(&self.back_focus, cx);
        cx.notify();
    }

    pub fn start_making(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dispatch(
            Input::DismissAction(ActionKind::CreateApp(self.workspace)),
            cx,
        );
        self.name.update(cx, |input, cx| input.set_text("", cx));
        self.scopes = AppScope::DEFAULT.to_vec();
        self.view = AppsView::Making;
        let handle = self.name.focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn make(&mut self, cx: &mut Context<Self>) {
        let kind = ActionKind::CreateApp(self.workspace);
        if self.action(kind, cx) == ActionState::InFlight {
            return;
        }
        let name = self.name.read(cx).text().trim().to_owned();
        self.waiting = Some((kind, false));
        self.dispatch(
            Input::CreateApp {
                workspace: self.workspace,
                name,
                scopes: self.scopes.clone(),
            },
            cx,
        );
    }

    /// The channels a bot could be added to: the person's open channels here, by name.
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
            .debug_selector(|| "apps-error".into())
            .text_size(theme.type_scale.small)
            .text_color(theme.colors.danger)
            .child(message)
            .into_any_element()
    }

    fn section_title(&self, theme: &Theme, text: &'static str) -> AnyElement {
        div()
            .text_size(theme.type_scale.small)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.colors.text_muted)
            .child(text)
            .into_any_element()
    }

    fn header(&self, theme: &Theme, title: String, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let view = self.view;
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(theme.type_scale.title)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.colors.text)
                    .child(title),
            )
            .child(form::button(
                theme,
                "apps-back",
                if view == AppsView::List {
                    "All integrations"
                } else {
                    "All apps"
                },
                ButtonKind::Secondary,
                true,
                &self.back_focus,
                move |window, cx| {
                    this.update(cx, |panel, cx| {
                        if panel.view == AppsView::List {
                            cx.emit(AppsPanelEvent::Leave);
                        } else {
                            panel.show_list(window, cx);
                        }
                    })
                    .ok();
                },
            ))
            .into_any_element()
    }

    fn list(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let load = ActionKind::LoadApps(self.workspace);
        let loading = self.action(load, cx) == ActionState::InFlight;
        let error = match self.action(load, cx) {
            ActionState::Failed(message) => Some(message),
            _ => None,
        };
        let apps = self.apps(cx);
        let mut body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.header(theme, "Apps".into(), cx))
            .child(self.note(
                theme,
                "apps-intro",
                "An app is a bot with a token. A program uses the token with Lazy's bot API to \
                 post in the channels you add the bot to, in Lazy's message format or Slack's \
                 (POST /v1/bot/messages, see DEPLOY.md).",
            ))
            .children(error.map(|m| self.error_line(theme, format!("The apps didn't load. {m}"))));
        match apps {
            None if loading => body = body.child(self.note(theme, "apps-loading", "Loading…")),
            None => {}
            Some(apps) if apps.is_empty() => {
                body = body.child(self.note(theme, "apps-none", "No apps yet."));
            }
            Some(apps) => {
                let mut list = div()
                    .id("apps-list")
                    .debug_selector(|| "apps-list".into())
                    .max_h(px(300.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_1();
                for (index, app) in apps.iter().enumerate() {
                    let id = app.id;
                    let focus = Self::handle_for(&mut self.manage_focus, id, cx);
                    let channels = match app.channels.len() {
                        0 => "in no channel".to_owned(),
                        1 => format!("in #{}", app.channels[0].name.as_str()),
                        n => format!("in {n} channels"),
                    };
                    let open_to = this.clone();
                    list = list.child(
                        div()
                            .debug_selector(|| "apps-row".into())
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_3()
                            .py_2()
                            .rounded(theme.radii.medium)
                            .border_1()
                            .border_color(colors.border)
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
                                            .child(app.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_size(theme.type_scale.small)
                                            .text_color(colors.text_muted)
                                            .child(format!(
                                                "{channels} · token …{}",
                                                app.token_hint
                                            )),
                                    ),
                            )
                            .child(form::button(
                                theme,
                                format!("apps-manage-{index}"),
                                "Manage",
                                ButtonKind::Secondary,
                                true,
                                &focus,
                                move |window, cx| {
                                    open_to
                                        .update(cx, |panel, cx| panel.show_details(id, window, cx))
                                        .ok();
                                },
                            )),
                    );
                }
                body = body.child(list);
            }
        }
        let make_to = this;
        body.child(div().flex().child(form::button(
            theme,
            "apps-make",
            "Make an app",
            ButtonKind::Primary,
            true,
            &self.make_focus,
            move |window, cx| {
                make_to
                    .update(cx, |panel, cx| panel.start_making(window, cx))
                    .ok();
            },
        )))
        .into_any_element()
    }

    /// The scope toggles, for making an app (`app` `None`) or changing one at once.
    fn scope_toggles(
        &mut self,
        theme: &Theme,
        current: &[AppScope],
        app: Option<AppId>,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let mut toggles = div().flex().flex_col().gap_1();
        for scope in AppScope::ALL {
            let focus = Self::handle_for(&mut self.scope_focus, scope, cx);
            let on = current.contains(&scope);
            let current = current.to_vec();
            let toggle_to = this.clone();
            toggles = toggles.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(form::button(
                        theme,
                        format!("apps-scope-{}", scope.as_str().replace(':', "-")),
                        if on {
                            format!("✓ {}", scope.label())
                        } else {
                            scope.label().to_owned()
                        },
                        if on {
                            ButtonKind::Primary
                        } else {
                            ButtonKind::Secondary
                        },
                        enabled,
                        &focus,
                        move |_, cx| {
                            let mut next = current.clone();
                            match next.iter().position(|s| *s == scope) {
                                Some(at) => {
                                    next.remove(at);
                                }
                                None => next.push(scope),
                            }
                            toggle_to
                                .update(cx, |panel, cx| match app {
                                    None => {
                                        panel.scopes = next;
                                        cx.notify();
                                    }
                                    Some(app) => panel
                                        .dispatch(Input::UpdateAppScopes { app, scopes: next }, cx),
                                })
                                .ok();
                        },
                    ))
                    .child(
                        div()
                            .font_family(fonts::MONO_FAMILY)
                            .text_size(theme.type_scale.small)
                            .text_color(theme.colors.text_muted)
                            .child(scope.as_str()),
                    ),
            );
        }
        toggles.into_any_element()
    }

    fn making(&mut self, theme: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let kind = ActionKind::CreateApp(self.workspace);
        let state = self.action(kind, cx);
        let working = state == ActionState::InFlight;
        let error = match state {
            ActionState::Failed(message) => Some(message),
            _ => None,
        };
        let ready =
            !working && !self.scopes.is_empty() && !self.name.read(cx).text().trim().is_empty();
        let scopes = self.scopes.clone();
        let cancel_to = this.clone();
        let make_to = this;
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.header(theme, "Make an app".into(), cx))
            .child(form::labeled_field(
                theme,
                "Name",
                &self.name,
                Some("Its bot's name in channels, with an APP badge.".into()),
                error.map(Into::into),
                window,
                cx,
            ))
            .child(self.section_title(theme, "What it may do"))
            .child(self.scope_toggles(theme, &scopes, None, !working, cx))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "apps-cancel",
                        "Cancel",
                        ButtonKind::Secondary,
                        true,
                        &self.cancel_focus,
                        move |window, cx| {
                            cancel_to
                                .update(cx, |panel, cx| panel.show_list(window, cx))
                                .ok();
                        },
                    ))
                    .child(form::button(
                        theme,
                        "apps-confirm-make",
                        if working { "Making…" } else { "Make" },
                        ButtonKind::Primary,
                        ready,
                        &self.confirm_focus,
                        move |_, cx| {
                            make_to.update(cx, |panel, cx| panel.make(cx)).ok();
                        },
                    )),
            )
            .into_any_element()
    }

    fn details(&mut self, id: AppId, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let Some(app) = self.app(id, cx) else {
            return div()
                .flex()
                .flex_col()
                .gap_3()
                .child(self.header(theme, "App".into(), cx))
                .child(self.note(theme, "apps-gone", "This app no longer exists."))
                .into_any_element();
        };
        let token = self
            .model
            .read(cx)
            .core()
            .app_token(id)
            .map(|t| t.as_str().to_owned());
        let mut body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.header(theme, app.name.clone(), cx))
            .child(self.section_title(theme, "Token"));
        if let Some(token) = token {
            let copy_to = this.clone();
            let shown = token.clone();
            body = body
                .child(self.note(
                    theme,
                    "apps-token-once",
                    "Copy the token now: Lazy keeps only a fingerprint of it and can't show it \
                     again. Send it as Authorization: Bearer <token>.",
                ))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .debug_selector(|| "apps-token".into())
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
                            "apps-copy-token",
                            if self.copied { "Copied" } else { "Copy" },
                            ButtonKind::Secondary,
                            true,
                            &self.copy_focus,
                            move |_, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(token.clone()));
                                copy_to
                                    .update(cx, |panel, cx| {
                                        panel.copied = true;
                                        cx.notify();
                                    })
                                    .ok();
                            },
                        )),
                );
        } else {
            let renew_to = this.clone();
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().child(self.note(
                        theme,
                        "apps-token-hint",
                        format!(
                            "A token ending in …{}. Lost it? Make a new one; the old one stops \
                             working at once.",
                            app.token_hint
                        ),
                    )))
                    .child(form::button(
                        theme,
                        "apps-new-token",
                        "New token…",
                        ButtonKind::Secondary,
                        true,
                        &self.new_token_focus,
                        move |window, cx| {
                            renew_to
                                .update(cx, |panel, cx| {
                                    panel.dispatch(
                                        Input::DismissAction(ActionKind::NewAppToken(id)),
                                        cx,
                                    );
                                    panel.view = AppsView::ConfirmNewToken(id);
                                    window.focus(&panel.cancel_focus, cx);
                                    cx.notify();
                                })
                                .ok();
                        },
                    )),
            );
        }

        // What it may do, changed at once.
        let scopes_state = self.action(ActionKind::UpdateAppScopes(id), cx);
        body = body
            .child(self.section_title(theme, "What it may do"))
            .child(self.scope_toggles(
                theme,
                &app.scopes,
                Some(id),
                scopes_state != ActionState::InFlight,
                cx,
            ))
            .children(match scopes_state {
                ActionState::Failed(message) => Some(self.error_line(theme, message)),
                _ => None,
            });

        // Channels: the ones it is in, then the ones it could join.
        body = body.child(self.section_title(theme, "Channels its bot is in"));
        let in_channels: Vec<ConversationId> =
            app.channels.iter().map(|c| c.conversation_id).collect();
        let mut chips = div().flex().flex_wrap().gap_1();
        for (index, (channel, name)) in self.channels(cx).into_iter().enumerate() {
            let focus = Self::handle_for(&mut self.channel_focus, channel, cx);
            let inside = in_channels.contains(&channel);
            let state = self.action(ActionKind::AppChannel(id, channel), cx);
            let toggle_to = this.clone();
            chips = chips.child(form::button(
                theme,
                format!("apps-channel-{index}"),
                if inside {
                    format!("✓ #{name}")
                } else {
                    format!("#{name}")
                },
                if inside {
                    ButtonKind::Primary
                } else {
                    ButtonKind::Secondary
                },
                state != ActionState::InFlight,
                &focus,
                move |_, cx| {
                    toggle_to
                        .update(cx, |panel, cx| {
                            panel.dispatch(
                                Input::SetAppChannel {
                                    app: id,
                                    conversation: channel,
                                    add: !inside,
                                },
                                cx,
                            )
                        })
                        .ok();
                },
            ));
        }
        body = body
            .child(self.note(
                theme,
                "apps-channels-hint",
                "Choose a channel to add the bot; choose it again to take it out.",
            ))
            .child(chips);
        let failures: Vec<String> = in_channels
            .iter()
            .chain(self.channel_focus.keys())
            .filter_map(|c| match self.action(ActionKind::AppChannel(id, *c), cx) {
                ActionState::Failed(message) => Some(message),
                _ => None,
            })
            .collect();
        if let Some(message) = failures.into_iter().next() {
            body = body.child(self.error_line(theme, message));
        }
        let delete_to = this;
        body.child(div().flex().justify_end().child(form::button(
            theme,
            "apps-delete",
            "Delete…",
            ButtonKind::Danger,
            true,
            &self.delete_focus,
            move |window, cx| {
                delete_to
                    .update(cx, |panel, cx| {
                        panel.dispatch(Input::DismissAction(ActionKind::DeleteApp(id)), cx);
                        panel.view = AppsView::ConfirmDelete(id);
                        window.focus(&panel.cancel_focus, cx);
                        cx.notify();
                    })
                    .ok();
            },
        )))
        .into_any_element()
    }

    fn confirm(
        &mut self,
        id: AppId,
        delete: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let name = self.app(id, cx).map(|a| a.name).unwrap_or_default();
        let kind = if delete {
            ActionKind::DeleteApp(id)
        } else {
            ActionKind::NewAppToken(id)
        };
        let state = self.action(kind, cx);
        let working = state == ActionState::InFlight;
        let error = match state {
            ActionState::Failed(message) => Some(message),
            _ => None,
        };
        let question = if delete {
            format!(
                "Delete {name}? Its token stops working at once and its bot leaves every channel. \
                 What it posted stays."
            )
        } else {
            format!(
                "Make a new token for {name}? The current one stops working at once; whatever uses \
                 it needs the new one."
            )
        };
        let cancel_to = this.clone();
        let go_to = this;
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.header(theme, name, cx))
            .child(
                div()
                    .debug_selector(|| "apps-question".into())
                    .text_size(theme.type_scale.body)
                    .text_color(theme.colors.text)
                    .child(question),
            )
            .children(error.map(|m| self.error_line(theme, m)))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "apps-confirm-cancel",
                        "Cancel",
                        ButtonKind::Secondary,
                        !working,
                        &self.cancel_focus,
                        move |window, cx| {
                            cancel_to
                                .update(cx, |panel, cx| panel.show_details(id, window, cx))
                                .ok();
                        },
                    ))
                    .child(form::button(
                        theme,
                        "apps-confirm",
                        match (delete, working) {
                            (true, true) => "Deleting…",
                            (true, false) => "Delete",
                            (false, true) => "Making…",
                            (false, false) => "Make a new token",
                        },
                        if delete {
                            ButtonKind::Danger
                        } else {
                            ButtonKind::Primary
                        },
                        !working,
                        &self.confirm_focus,
                        move |_, cx| {
                            go_to
                                .update(cx, |panel, cx| {
                                    panel.waiting = Some((kind, false));
                                    panel.dispatch(
                                        if delete {
                                            Input::DeleteApp(id)
                                        } else {
                                            Input::NewAppToken(id)
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

impl Render for AppsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        div()
            .debug_selector(|| "apps-panel".into())
            .child(match self.view {
                AppsView::List => self.list(&theme, cx),
                AppsView::Making => self.making(&theme, window, cx),
                AppsView::Details(id) => self.details(id, &theme, cx),
                AppsView::ConfirmNewToken(id) => self.confirm(id, false, &theme, cx),
                AppsView::ConfirmDelete(id) => self.confirm(id, true, &theme, cx),
            })
    }
}
