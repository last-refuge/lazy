//! Apps with bot tokens, for admins and owners (D35).
//!
//! The workspace's apps are loaded on request and replaced with the server's answer after each
//! change. A token just made is held in [`ClientCore::app_token`], kept out of debug output and
//! never saved, until it is dismissed: the server keeps only its hash.

use lazy_domain::app::AppScope;
use lazy_domain::{AppId, WorkspaceId, WorkspaceRole};
use lazy_protocol::{App, CreateAppRequest, UpdateAppRequest};

use super::conversations::readable_error;
use super::*;
use crate::model::{ActionOutcome, SecretUrl};

// ----- Reading -----

impl ClientCore {
    /// Whether to offer apps: admins and owners of the workspace. The server decides.
    pub fn can_manage_apps(&self, workspace: WorkspaceId) -> bool {
        self.role_in(workspace)
            .is_some_and(|role| role >= WorkspaceRole::Admin)
    }

    /// The workspace's apps as last loaded, or `None` if not loaded (or not an admin).
    pub fn apps(&self, workspace: WorkspaceId) -> Option<&[App]> {
        if !self.can_manage_apps(workspace) {
            return None;
        }
        self.apps.get(&workspace).map(Vec::as_slice)
    }

    /// The token just made for `app`, until dismissed.
    pub fn app_token(&self, app: AppId) -> Option<&SecretUrl> {
        self.app_token
            .as_ref()
            .filter(|(made_for, _)| *made_for == app)
            .map(|(_, token)| token)
    }

    /// The app a token was just made for, if any.
    pub fn app_token_for(&self) -> Option<AppId> {
        self.app_token.as_ref().map(|(app, _)| *app)
    }

    fn app_workspace(&self, app: AppId) -> Option<WorkspaceId> {
        self.apps
            .iter()
            .find(|(_, apps)| apps.iter().any(|a| a.id == app))
            .map(|(workspace, _)| *workspace)
    }
}

// ----- Acting -----

fn valid_scopes(scopes: Vec<AppScope>) -> Vec<AppScope> {
    let mut scopes: Vec<AppScope> = scopes
        .into_iter()
        .filter(|s| *s != AppScope::Unknown)
        .collect();
    scopes.sort_unstable();
    scopes.dedup();
    scopes
}

impl ClientCore {
    fn apps_ready(&self, workspace: WorkspaceId) -> bool {
        matches!(self.phase, SessionPhase::Ready) && self.can_manage_apps(workspace)
    }

    pub(super) fn load_apps(&mut self, workspace: WorkspaceId, out: &mut Output) {
        if !self.apps_ready(workspace) {
            return;
        }
        if self.begin_action(ActionKind::LoadApps(workspace), out) {
            out.effects.push(Effect::LoadApps { workspace });
        }
    }

    pub(super) fn create_app(
        &mut self,
        workspace: WorkspaceId,
        name: &str,
        scopes: Vec<AppScope>,
        out: &mut Output,
    ) {
        let kind = ActionKind::CreateApp(workspace);
        if !matches!(self.phase, SessionPhase::Ready) || self.action(kind) == &ActionState::InFlight
        {
            return;
        }
        if !self.can_manage_apps(workspace) {
            return self.fail_action(kind, "Only admins and owners can make apps.", out);
        }
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            return self.fail_action(kind, "Give the app a name of up to 64 characters.", out);
        }
        let scopes = valid_scopes(scopes);
        if scopes.is_empty() {
            return self.fail_action(kind, "Choose at least one thing the app may do.", out);
        }
        if self.begin_action(kind, out) {
            out.effects.push(Effect::CreateApp {
                workspace,
                request: CreateAppRequest {
                    name: name.to_owned(),
                    scopes,
                },
            });
        }
    }

    pub(super) fn new_app_token(&mut self, app: AppId, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        if self.begin_action(ActionKind::NewAppToken(app), out) {
            out.effects.push(Effect::NewAppToken { app });
        }
    }

    pub(super) fn update_app_scopes(
        &mut self,
        app: AppId,
        scopes: Vec<AppScope>,
        out: &mut Output,
    ) {
        let kind = ActionKind::UpdateAppScopes(app);
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        let scopes = valid_scopes(scopes);
        if scopes.is_empty() {
            return self.fail_action(kind, "An app needs at least one thing it may do.", out);
        }
        if self.begin_action(kind, out) {
            out.effects.push(Effect::UpdateAppScopes {
                app,
                request: UpdateAppRequest { scopes },
            });
        }
    }

    pub(super) fn set_app_channel(
        &mut self,
        app: AppId,
        conversation: ConversationId,
        add: bool,
        out: &mut Output,
    ) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        if self.begin_action(ActionKind::AppChannel(app, conversation), out) {
            out.effects.push(Effect::SetAppChannel {
                app,
                conversation,
                add,
            });
        }
    }

    pub(super) fn delete_app(&mut self, app: AppId, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        if self.begin_action(ActionKind::DeleteApp(app), out) {
            out.effects.push(Effect::DeleteApp { app });
        }
    }

    pub(super) fn dismiss_app_token(&mut self, out: &mut Output) {
        if self.app_token.take().is_some() {
            out.changes.push(Change::AppTokenChanged);
        }
    }

    /// Puts `app` in its workspace's list (replacing the old one, or adding it at the end).
    fn put_app(&mut self, workspace: WorkspaceId, app: App, out: &mut Output) {
        let list = self.apps.entry(workspace).or_default();
        match list.iter_mut().find(|a| a.id == app.id) {
            Some(old) => *old = app,
            None => list.push(app),
        }
        out.changes.push(Change::AppsChanged { workspace });
    }

    /// The outcome of an app request that succeeded.
    pub(super) fn app_action_result(
        &mut self,
        kind: ActionKind,
        outcome: ActionOutcome,
        out: &mut Output,
    ) {
        match (kind, outcome) {
            (ActionKind::LoadApps(workspace), ActionOutcome::Apps(apps)) => {
                self.apps.insert(workspace, apps);
                out.changes.push(Change::AppsChanged { workspace });
            }
            (ActionKind::CreateApp(workspace), ActionOutcome::AppToken(app, token)) => {
                self.app_token = Some((app.id, token));
                out.changes.push(Change::AppTokenChanged);
                self.put_app(workspace, *app, out);
            }
            (ActionKind::NewAppToken(id), ActionOutcome::AppToken(app, token)) => {
                self.app_token = Some((app.id, token));
                out.changes.push(Change::AppTokenChanged);
                if let Some(workspace) = self.app_workspace(id) {
                    self.put_app(workspace, *app, out);
                }
            }
            (
                ActionKind::UpdateAppScopes(id) | ActionKind::AppChannel(id, _),
                ActionOutcome::App(app),
            ) => {
                if let Some(workspace) = self.app_workspace(id) {
                    self.put_app(workspace, *app, out);
                }
            }
            (ActionKind::DeleteApp(id), _) => {
                if let Some(workspace) = self.app_workspace(id) {
                    if let Some(list) = self.apps.get_mut(&workspace) {
                        list.retain(|a| a.id != id);
                    }
                    out.changes.push(Change::AppsChanged { workspace });
                }
                if self.app_token_for() == Some(id) {
                    self.dismiss_app_token(out);
                }
            }
            _ => {}
        }
    }
}

/// Words for a person about why an app request failed.
pub(super) fn app_error(kind: ActionKind, error: &RequestError) -> String {
    match error {
        RequestError::Rejected(api) => match api.code {
            lazy_protocol::ErrorCode::Forbidden => {
                "Only admins and owners can manage apps.".to_owned()
            }
            lazy_protocol::ErrorCode::Conflict => match kind {
                ActionKind::AppChannel(..) => "That channel is archived.".to_owned(),
                _ => {
                    let mut text = api.message.clone();
                    if let Some(first) = text.get_mut(..1) {
                        first.make_ascii_uppercase();
                    }
                    if !text.ends_with('.') {
                        text.push('.');
                    }
                    text
                }
            },
            lazy_protocol::ErrorCode::InvalidRequest => {
                let mut text = api.message.clone();
                if let Some(first) = text.get_mut(..1) {
                    first.make_ascii_uppercase();
                }
                if !text.ends_with('.') {
                    text.push('.');
                }
                text
            }
            lazy_protocol::ErrorCode::NotFound => {
                "That no longer exists, or you can't see it.".to_owned()
            }
            _ => readable_error(error),
        },
        RequestError::Unknown(_) => readable_error(error),
    }
}
