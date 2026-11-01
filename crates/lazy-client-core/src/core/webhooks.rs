//! Incoming webhooks and bots (D16).
//!
//! An admin or owner creates a webhook for a conversation and is shown its address once; the
//! server keeps only a hash of the secret. The address is held in [`ClientCore::created_webhook`]
//! until it is dismissed, wrapped so that no debug output can contain it, and is never cached,
//! saved, or logged. A webhook has a bot: a member of the conversation with `is_bot` set. Bots
//! are never offered as people to message, mention, or put in a group, and nobody is the author
//! of their messages, so those cannot be edited (a moderator deletes them).

use lazy_domain::{MAX_WEBHOOKS_PER_CONVERSATION, WebhookId, WorkspaceId, WorkspaceRole};
use lazy_protocol::{CreateWebhookRequest, Webhook, WebhookDelivery, WorkspaceWebhook};

use super::conversations::readable_error;
use super::*;
use crate::model::{ActionOutcome, CreatedWebhook};

// ----- Reading -----

impl ClientCore {
    /// Whether a person is an app (the bot of a webhook), not a person. For the "APP" badge.
    pub fn is_bot(&self, user: UserId) -> bool {
        self.users.get(&user).is_some_and(|user| user.is_bot)
    }

    /// Whether to offer managing webhooks for a conversation: admins and owners of its
    /// workspace, in a conversation they belong to. The server decides.
    pub fn can_manage_webhooks(&self, conversation: ConversationId) -> bool {
        self.conversation(conversation)
            .and_then(|c| self.role_in(c.workspace_id))
            .is_some_and(|role| role >= WorkspaceRole::Admin)
    }

    /// A conversation's webhooks, oldest first, as last loaded (`Input::LoadWebhooks`) and kept
    /// current by this client's own creations and deletions. Empty for anyone who cannot manage
    /// them. Other admins' changes are seen the next time they are loaded.
    pub fn webhooks(&self, conversation: ConversationId) -> &[Webhook] {
        if !self.can_manage_webhooks(conversation) {
            return &[];
        }
        self.webhook_lists
            .get(&conversation)
            .map_or(&[], Vec::as_slice)
    }

    /// Whether to offer the workspace's list of integrations: its admins and owners. The server
    /// decides.
    pub fn can_manage_workspace_webhooks(&self, workspace: WorkspaceId) -> bool {
        self.role_in(workspace)
            .is_some_and(|role| role >= WorkspaceRole::Admin)
    }

    /// Every webhook of a workspace that the person can see, oldest first, each with its latest
    /// delivery, as last loaded (`Input::LoadWorkspaceWebhooks`) and kept current by this
    /// client's own creations, new addresses and deletions. Empty for anyone who cannot manage
    /// them.
    pub fn workspace_webhooks(&self, workspace: WorkspaceId) -> &[WorkspaceWebhook] {
        if !self.can_manage_workspace_webhooks(workspace) {
            return &[];
        }
        self.workspace_webhooks
            .get(&workspace)
            .map_or(&[], Vec::as_slice)
    }

    /// A webhook's delivery log, newest first, as last loaded (`Input::LoadWebhookDeliveries`),
    /// or `None` if it has not been loaded (or the person cannot manage it).
    pub fn webhook_deliveries(&self, webhook: WebhookId) -> Option<&[WebhookDelivery]> {
        if !self.can_manage_webhook(webhook) {
            return None;
        }
        self.webhook_deliveries.get(&webhook).map(Vec::as_slice)
    }

    /// The workspace a loaded webhook belongs to, from whichever list holds it.
    fn webhook_workspace(&self, webhook: WebhookId) -> Option<WorkspaceId> {
        self.workspace_webhooks
            .iter()
            .find(|(_, list)| list.iter().any(|held| held.webhook.id == webhook))
            .map(|(workspace, _)| *workspace)
            .or_else(|| {
                self.webhook_conversation(webhook)
                    .and_then(|conversation| self.conversation(conversation))
                    .map(|info| info.workspace_id)
            })
    }

    /// Whether the person may manage a webhook this client has loaded: an admin or owner of its
    /// workspace. The server decides.
    fn can_manage_webhook(&self, webhook: WebhookId) -> bool {
        self.webhook_workspace(webhook)
            .is_some_and(|workspace| self.can_manage_workspace_webhooks(workspace))
    }

    /// The webhook just created, with the address it can be posted to. The address is shown
    /// once; it stays here until `Input::DismissCreatedWebhook`.
    pub fn created_webhook(&self) -> Option<&CreatedWebhook> {
        self.created_webhook.as_ref()
    }
}

// ----- Acting -----

impl ClientCore {
    pub(super) fn create_webhook(
        &mut self,
        conversation: ConversationId,
        name: &str,
        out: &mut Output,
    ) {
        let kind = ActionKind::CreateWebhook(conversation);
        if !matches!(self.phase, SessionPhase::Ready) || self.action(kind) == &ActionState::InFlight
        {
            return;
        }
        let Some(info) = self.conversation(conversation) else {
            return;
        };
        let archived = info.archived;
        if !self.can_manage_webhooks(conversation) {
            return self.fail_action(kind, "Only admins and owners can manage webhooks.", out);
        }
        if archived {
            let text = "This conversation is archived, so it can't have webhooks.";
            return self.fail_action(kind, text, out);
        }
        let name = match DisplayName::parse(name.trim()) {
            Ok(name) => name,
            Err(error) => {
                let text = match error {
                    lazy_domain::NameError::TooShort { .. } => {
                        "Give the webhook a name.".to_owned()
                    }
                    lazy_domain::NameError::TooLong { max, .. } => {
                        format!("A webhook name can be at most {max} characters.")
                    }
                    _ => "A webhook name can't contain control characters.".to_owned(),
                };
                return self.fail_action(kind, text, out);
            }
        };
        if self
            .webhook_lists
            .get(&conversation)
            .is_some_and(|list| list.len() >= MAX_WEBHOOKS_PER_CONVERSATION)
        {
            let text = format!(
                "A conversation can have at most {MAX_WEBHOOKS_PER_CONVERSATION} webhooks. \
                 Delete one first."
            );
            return self.fail_action(kind, text, out);
        }
        if self.begin_action(kind, out) {
            out.effects.push(Effect::CreateWebhook {
                conversation,
                request: CreateWebhookRequest { name },
            });
        }
    }

    pub(super) fn delete_webhook(&mut self, webhook: WebhookId, out: &mut Output) {
        let kind = ActionKind::DeleteWebhook(webhook);
        if !matches!(self.phase, SessionPhase::Ready) || self.action(kind) == &ActionState::InFlight
        {
            return;
        }
        if self.webhook_workspace(webhook).is_none() {
            return;
        }
        if !self.can_manage_webhook(webhook) {
            return self.fail_action(kind, "Only admins and owners can manage webhooks.", out);
        }
        if self.begin_action(kind, out) {
            out.effects.push(Effect::DeleteWebhook { webhook });
        }
    }

    pub(super) fn load_workspace_webhooks(&mut self, workspace: WorkspaceId, out: &mut Output) {
        let kind = ActionKind::LoadWorkspaceWebhooks(workspace);
        if !matches!(self.phase, SessionPhase::Ready)
            || !self.can_manage_workspace_webhooks(workspace)
        {
            return;
        }
        if self.begin_action(kind, out) {
            out.effects
                .push(Effect::LoadWorkspaceWebhooks { workspace });
        }
    }

    pub(super) fn load_webhook_deliveries(&mut self, webhook: WebhookId, out: &mut Output) {
        let kind = ActionKind::LoadWebhookDeliveries(webhook);
        if !matches!(self.phase, SessionPhase::Ready) || !self.can_manage_webhook(webhook) {
            return;
        }
        if self.begin_action(kind, out) {
            out.effects.push(Effect::LoadWebhookDeliveries { webhook });
        }
    }

    pub(super) fn new_webhook_address(&mut self, webhook: WebhookId, out: &mut Output) {
        let kind = ActionKind::NewWebhookAddress(webhook);
        if !matches!(self.phase, SessionPhase::Ready) || self.action(kind) == &ActionState::InFlight
        {
            return;
        }
        if self.webhook_workspace(webhook).is_none() {
            return;
        }
        if !self.can_manage_webhook(webhook) {
            return self.fail_action(kind, "Only admins and owners can manage webhooks.", out);
        }
        if self.begin_action(kind, out) {
            out.effects.push(Effect::NewWebhookAddress { webhook });
        }
    }

    /// Asks again for a workspace's list, if it was ever loaded, after something changed it
    /// that this client cannot fill in by itself (a new webhook's latest delivery, or one whose
    /// creation may or may not have happened).
    pub(super) fn reload_workspace_webhooks_of(
        &mut self,
        conversation: ConversationId,
        out: &mut Output,
    ) {
        let workspace = self
            .conversation(conversation)
            .map(|info| info.workspace_id);
        if let Some(workspace) = workspace.filter(|w| self.workspace_webhooks.contains_key(w)) {
            self.load_workspace_webhooks(workspace, out);
        }
    }

    pub(super) fn load_webhooks(&mut self, conversation: ConversationId, out: &mut Output) {
        let kind = ActionKind::LoadWebhooks(conversation);
        if !matches!(self.phase, SessionPhase::Ready)
            || self.conversation(conversation).is_none()
            || !self.can_manage_webhooks(conversation)
        {
            return;
        }
        if self.begin_action(kind, out) {
            out.effects.push(Effect::LoadWebhooks { conversation });
        }
    }

    /// The conversation a loaded webhook belongs to.
    fn webhook_conversation(&self, webhook: WebhookId) -> Option<ConversationId> {
        self.webhook_lists
            .iter()
            .find(|(_, list)| list.iter().any(|held| held.id == webhook))
            .map(|(conversation, _)| *conversation)
    }

    pub(super) fn dismiss_created_webhook(&mut self, out: &mut Output) {
        if self.created_webhook.take().is_some() {
            out.changes.push(Change::CreatedWebhookChanged);
        }
    }

    /// Forgets everything held about webhooks (the session ended).
    pub(super) fn clear_webhooks(&mut self) {
        self.github.clear();
        self.github_setup_url = None;
        self.webhook_lists.clear();
        self.workspace_webhooks.clear();
        self.webhook_deliveries.clear();
        self.created_webhook = None;
    }

    /// A conversation the person no longer belongs to takes its webhooks with it, and the
    /// address shown for one of them.
    pub(super) fn forget_webhooks_of(&mut self, conversation: ConversationId, out: &mut Output) {
        self.webhook_lists.remove(&conversation);
        if self
            .created_webhook
            .as_ref()
            .is_some_and(|created| created.webhook.conversation_id == conversation)
        {
            self.created_webhook = None;
            out.changes.push(Change::CreatedWebhookChanged);
        }
    }

    /// The outcome of a webhook request that succeeded.
    pub(super) fn webhook_action_result(
        &mut self,
        kind: ActionKind,
        outcome: ActionOutcome,
        out: &mut Output,
    ) {
        match (kind, outcome) {
            (ActionKind::LoadWebhooks(conversation), ActionOutcome::Webhooks(list)) => {
                let list = list
                    .into_iter()
                    .filter(|webhook| webhook.conversation_id == conversation)
                    .collect();
                self.webhook_lists.insert(conversation, list);
                out.changes.push(Change::WebhooksChanged { conversation });
            }
            (
                ActionKind::LoadWorkspaceWebhooks(workspace),
                ActionOutcome::WorkspaceWebhooks(list),
            ) => {
                self.workspace_webhooks.insert(workspace, list);
                out.changes
                    .push(Change::WorkspaceWebhooksChanged { workspace });
            }
            (ActionKind::LoadWebhookDeliveries(webhook), ActionOutcome::WebhookDeliveries(log)) => {
                self.webhook_deliveries.insert(webhook, log);
                out.changes
                    .push(Change::WebhookDeliveriesChanged { webhook });
            }
            (ActionKind::NewWebhookAddress(webhook), ActionOutcome::WebhookCreated(created)) => {
                if created.webhook.id != webhook {
                    return;
                }
                self.created_webhook = Some(created);
                out.changes.push(Change::CreatedWebhookChanged);
            }
            (ActionKind::CreateWebhook(conversation), ActionOutcome::WebhookCreated(created)) => {
                if created.webhook.conversation_id != conversation {
                    return;
                }
                let webhook = created.webhook.clone();
                self.created_webhook = Some(created);
                out.changes.push(Change::CreatedWebhookChanged);
                self.reload_workspace_webhooks_of(conversation, out);
                match self.webhook_lists.get_mut(&conversation) {
                    Some(list) => {
                        if !list.iter().any(|held| held.id == webhook.id) {
                            list.push(webhook);
                        }
                        out.changes.push(Change::WebhooksChanged { conversation });
                    }
                    // The list was never loaded, so it cannot be completed by guessing.
                    None => self.load_webhooks(conversation, out),
                }
            }
            (ActionKind::DeleteWebhook(webhook), _) => self.webhook_deleted(webhook, out),
            _ => {}
        }
    }

    /// A webhook is gone (deleted, or already was): off its list, and off the screen if its
    /// address was still shown.
    pub(super) fn webhook_deleted(&mut self, webhook: WebhookId, out: &mut Output) {
        let mut changed = Vec::new();
        for (workspace, list) in &mut self.workspace_webhooks {
            let before = list.len();
            list.retain(|held| held.webhook.id != webhook);
            if list.len() != before {
                changed.push(*workspace);
            }
        }
        for workspace in changed {
            out.changes
                .push(Change::WorkspaceWebhooksChanged { workspace });
        }
        if self
            .created_webhook
            .as_ref()
            .is_some_and(|created| created.webhook.id == webhook)
        {
            self.created_webhook = None;
            out.changes.push(Change::CreatedWebhookChanged);
        }
        let Some(conversation) = self.webhook_conversation(webhook) else {
            return;
        };
        if let Some(list) = self.webhook_lists.get_mut(&conversation) {
            list.retain(|held| held.id != webhook);
        }
        out.changes.push(Change::WebhooksChanged { conversation });
    }
}

/// Words for a person about why a webhook request failed.
pub(super) fn webhook_error(kind: ActionKind, error: &RequestError) -> String {
    match error {
        RequestError::Unknown(_) if matches!(kind, ActionKind::NewWebhookAddress(_)) => {
            "Couldn't reach the server, so it isn't known whether the address was changed. \
             Ask for a new address again: the old one may already have stopped working."
                .to_owned()
        }
        RequestError::Unknown(_) if matches!(kind, ActionKind::CreateWebhook(_)) => {
            "Couldn't reach the server, so it isn't known whether the webhook was created. \
             Look at the list: one that you have no address for can be deleted."
                .to_owned()
        }
        RequestError::Rejected(api) => match api.code {
            lazy_protocol::ErrorCode::Forbidden => {
                "Only admins and owners can manage webhooks.".to_owned()
            }
            lazy_protocol::ErrorCode::NotFound => {
                "That conversation or webhook no longer exists, or you can't see it.".to_owned()
            }
            lazy_protocol::ErrorCode::Conflict => {
                "This conversation is archived, so it can't have webhooks.".to_owned()
            }
            lazy_protocol::ErrorCode::LimitReached => format!(
                "A conversation can have at most {MAX_WEBHOOKS_PER_CONVERSATION} webhooks. \
                 Delete one first."
            ),
            _ => readable_error(error),
        },
        RequestError::Unknown(_) => readable_error(error),
    }
}
