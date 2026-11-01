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

    /// An app's signing secret, once loaded for its admin.
    pub fn app_signing_secret(&self, app: AppId) -> Option<&SecretUrl> {
        self.app_signing_secret
            .as_ref()
            .filter(|(of, _)| *of == app)
            .map(|(_, secret)| secret)
    }

    /// The slash commands members can use in a workspace, as last loaded.
    pub fn commands(&self, workspace: WorkspaceId) -> Option<&[lazy_protocol::WorkspaceCommand]> {
        self.slash_commands.get(&workspace).map(Vec::as_slice)
    }

    /// The command being run (or last failed) in a conversation, as typed.
    pub fn last_command(&self, conversation: ConversationId) -> Option<&str> {
        self.last_command.get(&conversation).map(String::as_str)
    }

    /// The private reply to the last command run in a conversation, until dismissed.
    pub fn command_reply(
        &self,
        conversation: ConversationId,
    ) -> Option<&crate::model::CommandReply> {
        self.command_replies.get(&conversation)
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

    pub(super) fn load_app_signing_secret(&mut self, app: AppId, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        if self.begin_action(ActionKind::LoadAppSigningSecret(app), out) {
            out.effects.push(Effect::LoadAppSigningSecret { app });
        }
    }

    pub(super) fn add_app_command(
        &mut self,
        app: AppId,
        name: &str,
        url: &str,
        description: &str,
        usage_hint: &str,
        out: &mut Output,
    ) {
        let kind = ActionKind::AppCommand(app);
        if !matches!(self.phase, SessionPhase::Ready) || self.action(kind) == &ActionState::InFlight
        {
            return;
        }
        let name = name.trim().trim_start_matches('/').to_ascii_lowercase();
        if !lazy_domain::app::is_command_name(&name) {
            return self.fail_action(
                kind,
                "A command's name is 1 to 32 lower-case letters, digits, - and _.",
                out,
            );
        }
        let url = url.trim();
        if !(url.starts_with("https://")
            || url.starts_with("http://localhost")
            || url.starts_with("http://127.0.0.1"))
        {
            return self.fail_action(kind, "The address must start with https://.", out);
        }
        if self.begin_action(kind, out) {
            out.effects.push(Effect::AddAppCommand {
                app,
                request: lazy_protocol::CreateAppCommandRequest {
                    name,
                    url: url.to_owned(),
                    description: description.trim().to_owned(),
                    usage_hint: usage_hint.trim().to_owned(),
                },
            });
        }
    }

    pub(super) fn remove_app_command(
        &mut self,
        app: AppId,
        command: lazy_domain::AppCommandId,
        out: &mut Output,
    ) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        if self.begin_action(ActionKind::RemoveAppCommand(command), out) {
            // Shown at once; the list is loaded again when the removal is answered.
            if let Some(workspace) = self.app_workspace(app)
                && let Some(entry) = self
                    .apps
                    .get_mut(&workspace)
                    .and_then(|apps| apps.iter_mut().find(|a| a.id == app))
            {
                entry.commands.retain(|c| c.id != command);
                out.changes.push(Change::AppsChanged { workspace });
            }
            out.effects.push(Effect::RemoveAppCommand { command });
        }
    }

    pub(super) fn set_app_events(
        &mut self,
        app: AppId,
        url: &str,
        types: Vec<String>,
        skip_check: bool,
        out: &mut Output,
    ) {
        let kind = ActionKind::AppEvents(app);
        if !matches!(self.phase, SessionPhase::Ready) || self.action(kind) == &ActionState::InFlight
        {
            return;
        }
        let url = url.trim();
        if !url.is_empty() && types.is_empty() {
            return self.fail_action(kind, "Choose at least one kind of event.", out);
        }
        if self.begin_action(kind, out) {
            out.effects.push(Effect::SetAppEvents {
                app,
                request: lazy_protocol::SetAppEventsRequest {
                    url: url.to_owned(),
                    types,
                    skip_check,
                },
            });
        }
    }

    pub(super) fn load_commands(&mut self, workspace: WorkspaceId, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) || self.role_in(workspace).is_none() {
            return;
        }
        if self.begin_action(ActionKind::LoadCommands(workspace), out) {
            out.effects.push(Effect::LoadCommands { workspace });
        }
    }

    /// Loads the commands of `conversation`'s workspace if they are not loaded yet.
    pub(super) fn prefetch_commands(&mut self, conversation: ConversationId, out: &mut Output) {
        let Some(workspace) = self.conversation(conversation).map(|c| c.workspace_id) else {
            return;
        };
        if !self.slash_commands.contains_key(&workspace)
            && self.action(ActionKind::LoadCommands(workspace)) != &ActionState::InFlight
        {
            self.load_commands(workspace, out);
        }
    }

    /// If `text` names a command of the conversation's workspace, runs it and says so. A text that
    /// only looks like one (`/shrug`, or the commands are not loaded yet) is a message.
    pub(super) fn run_command_text(
        &mut self,
        conversation: ConversationId,
        text: &str,
        out: &mut Output,
    ) -> bool {
        let Some((name, rest)) = lazy_domain::app::parse_command(text) else {
            return false;
        };
        let Some(workspace) = self.conversation(conversation).map(|c| c.workspace_id) else {
            return false;
        };
        let known = self
            .slash_commands
            .get(&workspace)
            .is_some_and(|commands| commands.iter().any(|c| c.name == name));
        if !known {
            return false;
        }
        let kind = ActionKind::RunCommand(conversation);
        if self.action(kind) == &ActionState::InFlight {
            return true;
        }
        if self.command_replies.remove(&conversation).is_some() {
            out.changes
                .push(Change::CommandReplyChanged { conversation });
        }
        self.last_command
            .insert(conversation, text.trim().to_owned());
        if self.begin_action(kind, out) {
            out.effects.push(Effect::RunCommand {
                conversation,
                request: lazy_protocol::RunCommandRequest {
                    command: name,
                    text: rest,
                },
            });
        }
        true
    }

    pub(super) fn dismiss_command_reply(&mut self, conversation: ConversationId, out: &mut Output) {
        if self.command_replies.remove(&conversation).is_some() {
            out.changes
                .push(Change::CommandReplyChanged { conversation });
        }
        self.dismiss_action(ActionKind::RunCommand(conversation), out);
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
            (ActionKind::LoadAppSigningSecret(app), ActionOutcome::AppSigningSecret(secret)) => {
                self.app_signing_secret = Some((app, secret));
                out.changes.push(Change::AppSigningSecretChanged);
            }
            (ActionKind::AppEvents(id), ActionOutcome::App(app)) => {
                if let Some(workspace) = self.app_workspace(id) {
                    self.put_app(workspace, *app, out);
                }
            }
            (ActionKind::AppCommand(id), ActionOutcome::App(app)) => {
                if let Some(workspace) = self.app_workspace(id) {
                    self.put_app(workspace, *app, out);
                    // Members' list of commands changed too.
                    self.slash_commands.remove(&workspace);
                    out.changes.push(Change::CommandsChanged { workspace });
                }
            }
            (ActionKind::RemoveAppCommand(_), _) => {
                let loaded: Vec<WorkspaceId> = self.apps.keys().copied().collect();
                for workspace in loaded {
                    self.slash_commands.remove(&workspace);
                    out.changes.push(Change::CommandsChanged { workspace });
                }
            }
            (ActionKind::LoadCommands(workspace), ActionOutcome::Commands(commands)) => {
                self.slash_commands.insert(workspace, commands);
                out.changes.push(Change::CommandsChanged { workspace });
            }
            (ActionKind::RunCommand(conversation), ActionOutcome::CommandRan(ran)) => {
                let command = self.last_command.remove(&conversation).unwrap_or_default();
                if let Some(document) = ran.reply {
                    self.command_replies.insert(
                        conversation,
                        crate::model::CommandReply {
                            command,
                            app_name: ran.app_name,
                            document,
                        },
                    );
                    out.changes
                        .push(Change::CommandReplyChanged { conversation });
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

/// The server's words, as a sentence.
fn sentence(message: &str) -> String {
    let mut text = message.to_owned();
    if let Some(first) = text.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    if !text.ends_with('.') {
        text.push('.');
    }
    text
}

/// Words for a person about why an app request failed.
pub(super) fn app_error(kind: ActionKind, error: &RequestError) -> String {
    // A command's failure is the server's to explain: which app, and what to do.
    if let (
        ActionKind::RunCommand(_) | ActionKind::LoadAppSigningSecret(_) | ActionKind::AppEvents(_),
        RequestError::Rejected(api),
    ) = (kind, error)
        && matches!(
            api.code,
            lazy_protocol::ErrorCode::Unavailable
                | lazy_protocol::ErrorCode::NotFound
                | lazy_protocol::ErrorCode::Forbidden
                | lazy_protocol::ErrorCode::InvalidRequest
        )
    {
        return sentence(&api.message);
    }
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
