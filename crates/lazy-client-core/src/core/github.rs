//! The GitHub connector, for admins and owners (D32).
//!
//! The status (connection, installations, subscriptions, recent deliveries) is loaded on request
//! and loaded again after anything this client changes, since the server decides what the change
//! looks like (the bot joining a channel, a delivery queued again). Connecting answers with a
//! one-time address to open in a browser: it is held in [`ClientCore::github_setup_url`], wrapped
//! so no debug output contains it, never cached or saved, until it is dismissed.

use lazy_domain::github::{GithubEvent, is_login, normalize_repository};
use lazy_domain::{GithubSubscriptionId, WorkspaceId, WorkspaceRole};
use lazy_protocol::{
    CreateGithubSubscriptionRequest, GithubStatusResponse, StartGithubSetupRequest,
};

use super::conversations::readable_error;
use super::*;
use crate::model::{ActionOutcome, SecretUrl};

// ----- Reading -----

impl ClientCore {
    /// Whether to offer GitHub: admins and owners of the workspace. The server decides.
    pub fn can_manage_github(&self, workspace: WorkspaceId) -> bool {
        self.role_in(workspace)
            .is_some_and(|role| role >= WorkspaceRole::Admin)
    }

    /// The workspace's GitHub status as last loaded (`Input::LoadGithub`), or `None` if it has
    /// not been loaded or the person cannot manage it.
    pub fn github(&self, workspace: WorkspaceId) -> Option<&GithubStatusResponse> {
        if !self.can_manage_github(workspace) {
            return None;
        }
        self.github.get(&workspace)
    }

    /// The address that continues connecting GitHub in a browser, until dismissed.
    pub fn github_setup_url(&self) -> Option<&SecretUrl> {
        self.github_setup_url.as_ref()
    }
}

// ----- Acting -----

impl ClientCore {
    fn github_ready(&self, workspace: WorkspaceId) -> bool {
        matches!(self.phase, SessionPhase::Ready) && self.can_manage_github(workspace)
    }

    pub(super) fn load_github(&mut self, workspace: WorkspaceId, out: &mut Output) {
        if !self.github_ready(workspace) {
            return;
        }
        if self.begin_action(ActionKind::LoadGithub(workspace), out) {
            out.effects.push(Effect::LoadGithub { workspace });
        }
    }

    /// Loads again every workspace's status that was loaded (after a change whose workspace the
    /// answer does not name).
    fn reload_github(&mut self, out: &mut Output) {
        let loaded: Vec<WorkspaceId> = self.github.keys().copied().collect();
        for workspace in loaded {
            self.load_github(workspace, out);
        }
    }

    pub(super) fn start_github_setup(
        &mut self,
        workspace: WorkspaceId,
        organization: &str,
        out: &mut Output,
    ) {
        let kind = ActionKind::StartGithubSetup(workspace);
        if !matches!(self.phase, SessionPhase::Ready) || self.action(kind) == &ActionState::InFlight
        {
            return;
        }
        if !self.can_manage_github(workspace) {
            return self.fail_action(kind, "Only admins and owners can connect GitHub.", out);
        }
        let organization = organization.trim();
        if !is_login(organization) {
            return self.fail_action(
                kind,
                "Type the GitHub organization's name, as in github.com/name.",
                out,
            );
        }
        if self.begin_action(kind, out) {
            out.effects.push(Effect::StartGithubSetup {
                workspace,
                request: StartGithubSetupRequest {
                    organization: organization.to_owned(),
                },
            });
        }
    }

    pub(super) fn dismiss_github_setup(&mut self, out: &mut Output) {
        if self.github_setup_url.take().is_some() {
            out.changes.push(Change::GithubSetupChanged);
        }
    }

    pub(super) fn github_subscribe(
        &mut self,
        conversation: ConversationId,
        repository: &str,
        events: Vec<GithubEvent>,
        out: &mut Output,
    ) {
        let kind = ActionKind::GithubSubscribe(conversation);
        if !matches!(self.phase, SessionPhase::Ready) || self.action(kind) == &ActionState::InFlight
        {
            return;
        }
        let Some(workspace) = self.conversation(conversation).map(|c| c.workspace_id) else {
            return;
        };
        if !self.can_manage_github(workspace) {
            return self.fail_action(kind, "Only admins and owners can manage GitHub.", out);
        }
        let Some(repository) = normalize_repository(repository) else {
            return self.fail_action(
                kind,
                "Write the repository as owner/name, like last-refuge/lazy.",
                out,
            );
        };
        let events: Vec<GithubEvent> = events
            .into_iter()
            .filter(|event| *event != GithubEvent::Unknown)
            .collect();
        if events.is_empty() {
            return self.fail_action(kind, "Choose at least one kind of activity.", out);
        }
        if self.begin_action(kind, out) {
            out.effects.push(Effect::GithubSubscribe {
                conversation,
                request: CreateGithubSubscriptionRequest { repository, events },
            });
        }
    }

    pub(super) fn github_unsubscribe(&mut self, id: GithubSubscriptionId, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        if self.begin_action(ActionKind::GithubUnsubscribe(id), out) {
            out.effects
                .push(Effect::GithubUnsubscribe { subscription: id });
        }
    }

    pub(super) fn retry_github_delivery(
        &mut self,
        workspace: WorkspaceId,
        delivery: i64,
        out: &mut Output,
    ) {
        if !self.github_ready(workspace) {
            return;
        }
        if self.begin_action(ActionKind::RetryGithubDelivery(delivery), out) {
            out.effects.push(Effect::RetryGithubDelivery { delivery });
        }
    }

    pub(super) fn disconnect_github(&mut self, workspace: WorkspaceId, out: &mut Output) {
        if !self.github_ready(workspace) {
            return;
        }
        if self.begin_action(ActionKind::DisconnectGithub(workspace), out) {
            out.effects.push(Effect::DisconnectGithub { workspace });
        }
    }

    /// The outcome of a GitHub request that succeeded.
    pub(super) fn github_action_result(
        &mut self,
        kind: ActionKind,
        outcome: ActionOutcome,
        out: &mut Output,
    ) {
        match (kind, outcome) {
            (ActionKind::LoadGithub(workspace), ActionOutcome::Github(status)) => {
                self.github.insert(workspace, *status);
                out.changes.push(Change::GithubChanged { workspace });
            }
            (ActionKind::StartGithubSetup(_), ActionOutcome::GithubSetup(url)) => {
                self.github_setup_url = Some(url);
                out.changes.push(Change::GithubSetupChanged);
            }
            (ActionKind::GithubSubscribe(conversation), _) => {
                if let Some(workspace) = self.conversation(conversation).map(|c| c.workspace_id) {
                    self.load_github(workspace, out);
                }
            }
            (ActionKind::DisconnectGithub(workspace), _) => self.load_github(workspace, out),
            (ActionKind::GithubUnsubscribe(_) | ActionKind::RetryGithubDelivery(_), _) => {
                self.reload_github(out)
            }
            _ => {}
        }
    }
}

/// Words for a person about why a GitHub request failed.
pub(super) fn github_error(kind: ActionKind, error: &RequestError) -> String {
    match error {
        RequestError::Rejected(api) => match api.code {
            lazy_protocol::ErrorCode::Forbidden => {
                "Only admins and owners can manage GitHub.".to_owned()
            }
            lazy_protocol::ErrorCode::Unavailable
                if matches!(kind, ActionKind::StartGithubSetup(_)) =>
            {
                "This server can't hold GitHub's keys yet: whoever runs it needs to set \
                 LAZY_CREDENTIALS_KEY."
                    .to_owned()
            }
            lazy_protocol::ErrorCode::Conflict => match kind {
                ActionKind::StartGithubSetup(_) => {
                    "GitHub is already connected. Disconnect it first to connect another app."
                        .to_owned()
                }
                ActionKind::RetryGithubDelivery(_) => {
                    "That delivery is not failed any more, so there is nothing to retry.".to_owned()
                }
                ActionKind::GithubSubscribe(_) => {
                    "Connect GitHub first, and make sure the channel is not archived.".to_owned()
                }
                _ => readable_error(error),
            },
            // The server's own words say which repository or owner is the problem.
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
