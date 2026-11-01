//! Webhook connectors (Linear, Jira), for admins and owners (D34).
//!
//! A connector's status (its address, whether its secret is set, the subscriptions, recent
//! deliveries) is loaded on request and loaded again after anything this client changes, since the
//! server decides how the change looks. A signing secret the admin pastes is passed straight to
//! the server and not kept; one Lazy makes is held in [`ClientCore::connector_secret`], kept out of
//! debug output and never saved, until it is dismissed.

use lazy_domain::connector::ConnectorProvider;
use lazy_domain::{ConnectorSubscriptionId, WorkspaceId, WorkspaceRole};
use lazy_protocol::{ConnectorStatusResponse, CreateConnectorSubscriptionRequest};

use super::conversations::readable_error;
use super::*;
use crate::model::{ActionOutcome, SecretUrl};

// ----- Reading -----

impl ClientCore {
    /// Whether to offer connectors: admins and owners of the workspace. The server decides.
    pub fn can_manage_connectors(&self, workspace: WorkspaceId) -> bool {
        self.role_in(workspace)
            .is_some_and(|role| role >= WorkspaceRole::Admin)
    }

    /// A connector's status as last loaded, or `None` if it has not been loaded or the person
    /// cannot manage it.
    pub fn connector(
        &self,
        workspace: WorkspaceId,
        provider: ConnectorProvider,
    ) -> Option<&ConnectorStatusResponse> {
        if !self.can_manage_connectors(workspace) {
            return None;
        }
        self.connectors.get(&(workspace, provider))
    }

    /// The signing secret Lazy just made for `provider`, until dismissed.
    pub fn connector_secret(&self, provider: ConnectorProvider) -> Option<&SecretUrl> {
        self.connector_secret
            .as_ref()
            .filter(|(made_for, _)| *made_for == provider)
            .map(|(_, secret)| secret)
    }
}

// ----- Acting -----

impl ClientCore {
    fn connector_ready(&self, workspace: WorkspaceId) -> bool {
        matches!(self.phase, SessionPhase::Ready) && self.can_manage_connectors(workspace)
    }

    pub(super) fn load_connector(
        &mut self,
        workspace: WorkspaceId,
        provider: ConnectorProvider,
        out: &mut Output,
    ) {
        if !self.connector_ready(workspace) || provider == ConnectorProvider::Unknown {
            return;
        }
        if self.begin_action(ActionKind::LoadConnector(workspace, provider), out) {
            out.effects.push(Effect::LoadConnector {
                workspace,
                provider,
            });
        }
    }

    /// Loads again every connector status that was loaded (after a change whose workspace or
    /// provider the answer does not name).
    fn reload_connectors(&mut self, out: &mut Output) {
        let loaded: Vec<(WorkspaceId, ConnectorProvider)> =
            self.connectors.keys().copied().collect();
        for (workspace, provider) in loaded {
            self.load_connector(workspace, provider, out);
        }
    }

    pub(super) fn connect_connector(
        &mut self,
        workspace: WorkspaceId,
        provider: ConnectorProvider,
        out: &mut Output,
    ) {
        let kind = ActionKind::ConnectConnector(workspace, provider);
        if !matches!(self.phase, SessionPhase::Ready) || provider == ConnectorProvider::Unknown {
            return;
        }
        if !self.can_manage_connectors(workspace) {
            return self.fail_action(kind, "Only admins and owners can connect apps.", out);
        }
        if self.begin_action(kind, out) {
            out.effects.push(Effect::ConnectConnector {
                workspace,
                provider,
            });
        }
    }

    pub(super) fn set_connector_secret(
        &mut self,
        workspace: WorkspaceId,
        provider: ConnectorProvider,
        secret: SecretUrl,
        out: &mut Output,
    ) {
        let kind = ActionKind::SetConnectorSecret(workspace, provider);
        if !self.connector_ready(workspace) || self.action(kind) == &ActionState::InFlight {
            return;
        }
        let text = secret.as_str().trim();
        if text.chars().count() < 8 || text.chars().any(char::is_whitespace) {
            return self.fail_action(
                kind,
                format!(
                    "Paste the whole signing secret from {}: it has no spaces.",
                    provider.label()
                ),
                out,
            );
        }
        let secret = SecretUrl::new(text.to_owned());
        if self.begin_action(kind, out) {
            out.effects.push(Effect::SetConnectorSecret {
                workspace,
                provider,
                secret,
            });
        }
    }

    pub(super) fn generate_connector_secret(
        &mut self,
        workspace: WorkspaceId,
        provider: ConnectorProvider,
        out: &mut Output,
    ) {
        let kind = ActionKind::SetConnectorSecret(workspace, provider);
        if !self.connector_ready(workspace) || self.action(kind) == &ActionState::InFlight {
            return;
        }
        if self.begin_action(kind, out) {
            out.effects.push(Effect::GenerateConnectorSecret {
                workspace,
                provider,
            });
        }
    }

    pub(super) fn dismiss_connector_secret(&mut self, out: &mut Output) {
        if self.connector_secret.take().is_some() {
            out.changes.push(Change::ConnectorSecretChanged);
        }
    }

    pub(super) fn connector_subscribe(
        &mut self,
        conversation: ConversationId,
        provider: ConnectorProvider,
        scope: &str,
        events: Vec<String>,
        out: &mut Output,
    ) {
        let kind = ActionKind::ConnectorSubscribe(conversation, provider);
        if !matches!(self.phase, SessionPhase::Ready) || self.action(kind) == &ActionState::InFlight
        {
            return;
        }
        let Some(workspace) = self.conversation(conversation).map(|c| c.workspace_id) else {
            return;
        };
        if !self.can_manage_connectors(workspace) {
            return self.fail_action(kind, "Only admins and owners can manage apps.", out);
        }
        let Some(scope) = provider.normalize_scope(scope) else {
            return self.fail_action(
                kind,
                format!(
                    "Type a {} key like {}, or leave it empty to follow everything.",
                    provider.scope_label().to_lowercase(),
                    provider.scope_example()
                ),
                out,
            );
        };
        let events: Vec<String> = events
            .into_iter()
            .filter(|event| provider.kind(event).is_some())
            .collect();
        if events.is_empty() {
            return self.fail_action(kind, "Choose at least one kind of activity.", out);
        }
        if self.begin_action(kind, out) {
            out.effects.push(Effect::ConnectorSubscribe {
                conversation,
                provider,
                request: CreateConnectorSubscriptionRequest { scope, events },
            });
        }
    }

    pub(super) fn connector_unsubscribe(&mut self, id: ConnectorSubscriptionId, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        if self.begin_action(ActionKind::ConnectorUnsubscribe(id), out) {
            out.effects
                .push(Effect::ConnectorUnsubscribe { subscription: id });
        }
    }

    pub(super) fn retry_connector_delivery(
        &mut self,
        workspace: WorkspaceId,
        delivery: i64,
        out: &mut Output,
    ) {
        if !self.connector_ready(workspace) {
            return;
        }
        if self.begin_action(ActionKind::RetryConnectorDelivery(delivery), out) {
            out.effects
                .push(Effect::RetryConnectorDelivery { delivery });
        }
    }

    pub(super) fn disconnect_connector(
        &mut self,
        workspace: WorkspaceId,
        provider: ConnectorProvider,
        out: &mut Output,
    ) {
        if !self.connector_ready(workspace) {
            return;
        }
        if self.begin_action(ActionKind::DisconnectConnector(workspace, provider), out) {
            out.effects.push(Effect::DisconnectConnector {
                workspace,
                provider,
            });
        }
    }

    /// The outcome of a connector request that succeeded.
    pub(super) fn connector_action_result(
        &mut self,
        kind: ActionKind,
        outcome: ActionOutcome,
        out: &mut Output,
    ) {
        match (kind, outcome) {
            (
                ActionKind::LoadConnector(workspace, provider)
                | ActionKind::ConnectConnector(workspace, provider),
                ActionOutcome::Connector(status),
            ) => {
                self.connectors.insert((workspace, provider), *status);
                out.changes.push(Change::ConnectorChanged {
                    workspace,
                    provider,
                });
            }
            (ActionKind::SetConnectorSecret(workspace, provider), outcome) => {
                if let ActionOutcome::ConnectorSecret(secret) = outcome {
                    self.connector_secret = Some((provider, secret));
                    out.changes.push(Change::ConnectorSecretChanged);
                }
                self.load_connector(workspace, provider, out);
            }
            (ActionKind::ConnectorSubscribe(conversation, provider), _) => {
                if let Some(workspace) = self.conversation(conversation).map(|c| c.workspace_id) {
                    self.load_connector(workspace, provider, out);
                }
            }
            (ActionKind::DisconnectConnector(workspace, provider), _) => {
                self.load_connector(workspace, provider, out)
            }
            (ActionKind::ConnectorUnsubscribe(_) | ActionKind::RetryConnectorDelivery(_), _) => {
                self.reload_connectors(out)
            }
            _ => {}
        }
    }
}

/// Words for a person about why a connector request failed.
pub(super) fn connector_error(kind: ActionKind, error: &RequestError) -> String {
    match error {
        RequestError::Rejected(api) => match api.code {
            lazy_protocol::ErrorCode::Forbidden => {
                "Only admins and owners can manage apps.".to_owned()
            }
            lazy_protocol::ErrorCode::Unavailable => {
                "This server can't hold signing secrets yet: whoever runs it needs to set \
                 LAZY_CREDENTIALS_KEY."
                    .to_owned()
            }
            lazy_protocol::ErrorCode::Conflict => match kind {
                ActionKind::RetryConnectorDelivery(_) => {
                    "That delivery is not failed any more, so there is nothing to retry.".to_owned()
                }
                ActionKind::ConnectorSubscribe(..) => {
                    "Connect it first, and make sure the channel is not archived.".to_owned()
                }
                ActionKind::SetConnectorSecret(..) => "Connect it first.".to_owned(),
                _ => readable_error(error),
            },
            // The server's own words say what is wrong with the key or the secret.
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
