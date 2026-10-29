//! Conversations as the sidebar and dialogs see them: titles, sections, what to offer, and the
//! management actions (create, open, browse, join, leave, and the rest).
//!
//! Read state (unread counts, mark-read, the "New messages" divider) lives in
//! [`super::readstate`].

use lazy_domain::conversation::{MAX_PARTICIPANTS, description_is_valid, topic_is_valid};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationFacts, ConversationId, ConversationKind, NameError,
    OperationId, ParticipantError, ParticipantSet, UserId, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    AddMembersRequest, BrowsedChannel, Conversation, CreateChannelRequest, CreateGroupRequest,
    ErrorCode, OpenConversationRequest, UpdateConversationRequest, User,
};

use super::*;
use crate::{ActionOutcome, Sidebar};

/// A create or open request whose outcome is not settled yet.
#[derive(Debug)]
pub(super) struct PendingCommand {
    kind: ActionKind,
    workspace: WorkspaceId,
    request: CommandRequest,
    /// Resends triggered by "no record" answers.
    resends: u32,
    stage: CommandStage,
}

impl PendingCommand {
    pub(super) fn workspace(&self) -> WorkspaceId {
        self.workspace
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CommandRequest {
    Create(CreateChannelRequest),
    Open(OpenConversationRequest),
    Group(CreateGroupRequest),
}

impl CommandRequest {
    fn operation(&self) -> OperationId {
        match self {
            Self::Create(request) => request.operation_id,
            Self::Open(request) => request.operation_id,
            Self::Group(request) => request.operation_id,
        }
    }

    /// Whether both ask for the same thing, ignoring the operation id.
    fn same_content(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Create(a), Self::Create(b)) => {
                let mut b = b.clone();
                b.operation_id = a.operation_id;
                *a == b
            }
            (Self::Open(a), Self::Open(b)) => {
                let (mut x, mut y) = (a.participants.clone(), b.participants.clone());
                x.sort();
                y.sort();
                x == y
            }
            (Self::Group(a), Self::Group(b)) => {
                let mut b = b.clone();
                b.operation_id = a.operation_id;
                *a == b
            }
            _ => false,
        }
    }

    fn effect(&self, workspace: WorkspaceId) -> Effect {
        match self {
            Self::Create(request) => Effect::CreateChannel {
                workspace,
                request: request.clone(),
            },
            Self::Open(request) => Effect::OpenConversation {
                workspace,
                request: request.clone(),
            },
            Self::Group(request) => Effect::CreateGroup {
                workspace,
                request: request.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandStage {
    /// The request is in flight.
    Sending,
    /// Its outcome is unknown and the server is being asked.
    Querying,
    /// Given up for now; the person may retry, which asks the server first.
    Failed,
}

static IDLE: ActionState = ActionState::Idle;

// ----- Read model -----

impl ClientCore {
    /// What to call a conversation: a channel's name (without `#`), or the other participants'
    /// display names, alphabetical, joined with commas. Empty for an unknown conversation.
    pub fn conversation_title(&self, id: ConversationId) -> String {
        match self.conversation(id) {
            Some(conversation) => self.title_of(conversation),
            None => String::new(),
        }
    }

    /// The title of a conversation the core may not hold (a search hit in a channel the person has
    /// not joined): the channel's name, or the other people's names.
    pub(super) fn title_of(&self, conversation: &Conversation) -> String {
        match conversation.kind {
            ConversationKind::Channel => conversation
                .name
                .as_ref()
                .map_or_else(|| "channel".to_owned(), ToString::to_string),
            ConversationKind::Direct | ConversationKind::Group => {
                let me = self.me.as_ref().map(|user| user.id);
                let mut names: Vec<&str> = conversation
                    .participants
                    .iter()
                    .filter(|user| Some(**user) != me)
                    .map(|user| self.display_name(*user))
                    .collect();
                // The server lists participants in id order, which means nothing to a reader.
                names.sort_by_key(|name| name.to_lowercase());
                if names.is_empty() {
                    match conversation.kind {
                        ConversationKind::Direct => "Direct message".to_owned(),
                        _ => "Group".to_owned(),
                    }
                } else {
                    names.join(", ")
                }
            }
        }
    }

    /// The people of the workspace the client knows of, from the bootstrap and from people who
    /// joined since, including the signed-in person. Apps (bots) and deactivated people are left
    /// out: they are never offered as people to message or to put in a group, and `member(id)`
    /// and `display_name(id)` still know their names for showing their messages. Ordered by display
    /// name (ties by id), so the order is stable. For pickers; filter yourself out if you do
    /// not want to be listed.
    pub fn members(&self) -> Vec<&User> {
        // Apps are not people, and a deactivated person cannot be reached: neither is offered to
        // message or to put in a group.
        let mut members: Vec<&User> = self
            .users
            .values()
            .filter(|user| !user.is_bot && !user.deactivated)
            .collect();
        members.sort_by(|a, b| {
            a.display_name
                .as_str()
                .to_lowercase()
                .cmp(&b.display_name.as_str().to_lowercase())
                .then(a.id.cmp(&b.id))
        });
        members
    }

    /// The person's favorite flag, read position, and unread counts for a conversation they
    /// belong to.
    pub fn user_state(&self, id: ConversationId) -> Option<&ConversationUserState> {
        self.user_states.get(&id)
    }

    /// Unread messages across every conversation, for a badge in the window or dock.
    pub fn total_unread(&self) -> u32 {
        self.conversations.iter().fold(0u32, |sum, conversation| {
            let unread = self
                .user_states
                .get(&conversation.id)
                .map_or(0, |state| state.unread_count);
            sum.saturating_add(unread)
        })
    }

    /// Whether the window is considered focused. Assumed true until the UI says otherwise.
    pub fn window_active(&self) -> bool {
        self.window_active
    }

    /// Every conversation the person belongs to, arranged into the sidebar's sections.
    pub fn sidebar(&self) -> Sidebar {
        let mut favorites = Vec::new();
        let mut channels = Vec::new();
        let mut people = Vec::new();
        for conversation in &self.conversations {
            let favorite = self
                .user_states
                .get(&conversation.id)
                .is_some_and(|state| state.favorite);
            if favorite {
                favorites.push(conversation.id);
            } else if conversation.kind == ConversationKind::Channel {
                channels.push(conversation.id);
            } else {
                people.push(conversation.id);
            }
        }
        let by_title = |a: &ConversationId, b: &ConversationId| {
            self.conversation_title(*a)
                .to_lowercase()
                .cmp(&self.conversation_title(*b).to_lowercase())
                .then(a.cmp(b))
        };
        favorites.sort_by(by_title);
        channels.sort_by(by_title);
        people.sort_by(|a, b| {
            self.recency(*b)
                .cmp(&self.recency(*a))
                .then_with(|| by_title(a, b))
        });
        Sidebar {
            favorites,
            channels,
            direct_and_group: people,
        }
    }

    /// Favorites of any kind, ordered by title.
    pub fn favorites(&self) -> Vec<ConversationId> {
        self.sidebar().favorites
    }

    /// Channels that are not favorites, ordered by name.
    pub fn channels(&self) -> Vec<ConversationId> {
        self.sidebar().channels
    }

    /// Direct and group conversations that are not favorites, most recently active first.
    pub fn direct_and_group(&self) -> Vec<ConversationId> {
        self.sidebar().direct_and_group
    }

    /// The newest message position known for a conversation. Messages that have not been loaded
    /// or received are not known, so until history loads the read position stands in.
    fn recency(&self, id: ConversationId) -> EventSeq {
        let newest = self.timelines.get(&id).and_then(Timeline::newest_seq);
        let read = self.user_states.get(&id).map(|state| state.last_read);
        newest.max(read).unwrap_or(EventSeq::ZERO)
    }

    /// Progress of a management action.
    pub fn action(&self, kind: ActionKind) -> &ActionState {
        self.actions.get(&kind).unwrap_or(&IDLE)
    }

    // ----- What to offer -----

    pub(super) fn role_in(&self, workspace: WorkspaceId) -> Option<WorkspaceRole> {
        self.workspaces
            .iter()
            .find(|info| info.workspace.id == workspace)
            .map(|info| info.role)
    }

    fn facts_for(&self, conversation: &Conversation, is_member: bool) -> ConversationFacts {
        let me = self.me.as_ref().map(|user| user.id);
        ConversationFacts {
            workspace_role: self.role_in(conversation.workspace_id),
            is_member,
            is_creator: conversation.created_by.is_some() && conversation.created_by == me,
            kind: conversation.kind,
            visibility: conversation.visibility,
            archived: conversation.archived,
            is_default: conversation.is_default,
        }
    }

    /// The facts the management rules need, for a conversation the person belongs to.
    pub fn facts(&self, id: ConversationId) -> Option<ConversationFacts> {
        self.conversation(id)
            .map(|conversation| self.facts_for(conversation, true))
    }

    /// The facts for a directory entry, which the person may or may not have joined.
    pub fn browsed_facts(&self, channel: &BrowsedChannel) -> ConversationFacts {
        self.facts_for(&channel.conversation, channel.joined)
    }

    /// Whether to offer "Join" for a directory entry.
    pub fn can_join(&self, channel: &BrowsedChannel) -> bool {
        self.browsed_facts(channel).can_join()
    }

    /// Whether to offer "Leave". The server decides; this only avoids offering the impossible.
    pub fn can_leave(&self, id: ConversationId) -> bool {
        self.facts(id).is_some_and(|facts| facts.can_leave())
    }

    pub fn can_add_members(&self, id: ConversationId) -> bool {
        self.facts(id).is_some_and(|facts| facts.can_add_members())
    }

    pub fn can_remove_members(&self, id: ConversationId) -> bool {
        self.facts(id)
            .is_some_and(|facts| facts.can_remove_members())
    }

    /// Whether to offer renaming.
    pub fn can_rename(&self, id: ConversationId) -> bool {
        self.facts(id).is_some_and(|facts| facts.can_rename())
    }

    /// Whether to offer editing the topic and description.
    pub fn can_edit_details(&self, id: ConversationId) -> bool {
        self.facts(id).is_some_and(|facts| facts.can_edit_details())
    }

    pub fn can_archive(&self, id: ConversationId) -> bool {
        self.facts(id).is_some_and(|facts| facts.can_archive())
    }

    pub fn can_unarchive(&self, id: ConversationId) -> bool {
        self.facts(id).is_some_and(|facts| facts.can_unarchive())
    }
}

// ----- Action bookkeeping -----

impl ClientCore {
    pub(super) fn set_action(&mut self, kind: ActionKind, state: ActionState, out: &mut Output) {
        if self.action(kind) == &state {
            return;
        }
        if state == ActionState::Idle {
            self.actions.remove(&kind);
        } else {
            if state == ActionState::InFlight
                && let Some(scope) = self.scope
            {
                self.action_scope.insert(kind, scope);
            }
            self.actions.insert(kind, state);
        }
        out.changes.push(Change::ActionChanged(kind));
    }

    /// Marks an action started. `false` means it is already in flight and the input is ignored,
    /// so a double click cannot send twice.
    pub(super) fn begin_action(&mut self, kind: ActionKind, out: &mut Output) -> bool {
        if self.action(kind) == &ActionState::InFlight {
            return false;
        }
        self.set_action(kind, ActionState::InFlight, out);
        true
    }

    pub(super) fn fail_action(
        &mut self,
        kind: ActionKind,
        message: impl Into<String>,
        out: &mut Output,
    ) {
        self.set_action(kind, ActionState::Failed(message.into()), out);
    }

    /// Reports a failed request. A rejected session ends the session instead.
    pub(super) fn fail_request(
        &mut self,
        kind: ActionKind,
        error: &RequestError,
        out: &mut Output,
    ) {
        self.note_request_error("action", error);
        if error.is_unauthenticated() {
            self.set_action(kind, ActionState::Idle, out);
            self.session_expired(out);
        } else {
            let text = match kind {
                ActionKind::CreateGroup
                | ActionKind::UpdateGroup(_)
                | ActionKind::DeleteGroup(_) => super::groups::group_error(error),
                ActionKind::CreateWebhook(_)
                | ActionKind::DeleteWebhook(_)
                | ActionKind::LoadWebhooks(_) => super::webhooks::webhook_error(kind, error),
                ActionKind::AddCustomEmoji
                | ActionKind::RenameCustomEmoji(_)
                | ActionKind::DeleteCustomEmoji(_) => super::emoji::emoji_error(error),
                ActionKind::UpdateWorkspace
                | ActionKind::SetMemberRole(_)
                | ActionKind::SetMemberDeactivated(_)
                | ActionKind::CreateInvitation
                | ActionKind::RevokeInvitation(_) => super::admin::admin_error(kind, error),
                ActionKind::CreateWorkspace
                | ActionKind::AcceptInvitation
                | ActionKind::SetAvatar => super::workspaces::workspace_error(kind, error),
                _ => readable_error(error),
            };
            self.fail_action(kind, text, out);
        }
    }

    pub(super) fn dismiss_action(&mut self, kind: ActionKind, out: &mut Output) {
        if matches!(self.action(kind), ActionState::Failed(_)) {
            self.set_action(kind, ActionState::Idle, out);
        }
    }

    pub(super) fn primary_workspace_id(&self) -> Option<WorkspaceId> {
        self.scope
    }

    /// The conversation selected when nothing valid is: the default channel, then `#general`,
    /// then the first channel by name, then anything.
    pub(super) fn default_selection(&self) -> Option<ConversationId> {
        // The conversation the person last had open, if they are still in it, comes first.
        if let Some(restored) = self.restored_selection() {
            return Some(restored);
        }
        let sidebar = self.sidebar();
        self.conversations
            .iter()
            .find(|c| c.is_default)
            .or_else(|| {
                self.conversations
                    .iter()
                    .find(|c| c.name.as_ref().is_some_and(|n| n.as_str() == "general"))
            })
            .map(|c| c.id)
            .or_else(|| sidebar.channels.first().copied())
            .or_else(|| sidebar.favorites.first().copied())
            .or_else(|| sidebar.direct_and_group.first().copied())
    }

    /// Adds a conversation to the directory, or replaces what is known of it. Returns whether it
    /// was new.
    pub(super) fn adopt_conversation(
        &mut self,
        conversation: Conversation,
        out: &mut Output,
    ) -> bool {
        let id = conversation.id;
        self.sync_browse(&conversation, None);
        let is_new = if let Some(existing) = self.conversations.iter_mut().find(|c| c.id == id) {
            *existing = conversation;
            false
        } else {
            let as_of = self
                .applied
                .get(&conversation.workspace_id)
                .copied()
                .unwrap_or(EventSeq::ZERO);
            self.conversations.push(conversation);
            self.user_states
                .entry(id)
                .or_insert_with(|| default_user_state(id));
            self.state_as_of.entry(id).or_insert(as_of);
            true
        };
        out.changes.push(Change::DirectoryChanged);
        out.changes
            .push(Change::ConversationStateChanged { conversation: id });
        self.cache_changed(true);
        is_new
    }

    /// Replaces a conversation's snapshot, if it is known. Returns whether it was.
    pub(super) fn replace_conversation(
        &mut self,
        conversation: Conversation,
        out: &mut Output,
    ) -> bool {
        let id = conversation.id;
        self.sync_browse(&conversation, None);
        let Some(existing) = self.conversations.iter_mut().find(|c| c.id == id) else {
            return false;
        };
        *existing = conversation;
        out.changes.push(Change::DirectoryChanged);
        out.changes
            .push(Change::ConversationStateChanged { conversation: id });
        self.cache_changed(true);
        true
    }
}

// ----- Create and open: actions with an operation id -----

impl ClientCore {
    pub(super) fn create_channel(
        &mut self,
        name: &str,
        visibility: ChannelVisibility,
        topic: &str,
        description: &str,
        out: &mut Output,
    ) {
        let kind = ActionKind::CreateChannel;
        if self.action(kind) == &ActionState::InFlight {
            return;
        }
        // The workspace's policy decides who may create channels; the server enforces it too.
        if self.my_role().is_some() && !self.can_create_channel() {
            let message = "Only admins and owners can create channels in this workspace.";
            return self.fail_action(kind, message, out);
        }
        let name = match ChannelName::parse(&normalize_channel_name(name)) {
            Ok(name) => name,
            Err(error) => return self.fail_action(kind, describe_name_error(&error), out),
        };
        let topic = non_empty(topic);
        if topic.as_deref().is_some_and(|text| !topic_is_valid(text)) {
            let message = "A topic can be at most 250 characters and can't contain line breaks.";
            return self.fail_action(kind, message, out);
        }
        let description = non_empty(description);
        if description
            .as_deref()
            .is_some_and(|text| !description_is_valid(text))
        {
            let message = "A description can be at most 1,000 characters.";
            return self.fail_action(kind, message, out);
        }
        let Some(workspace) = self.primary_workspace_id() else {
            return self.fail_action(kind, "Your workspace hasn't loaded yet.", out);
        };
        let request = CommandRequest::Create(CreateChannelRequest {
            operation_id: OperationId::generate(),
            name,
            visibility,
            topic,
            description,
        });
        self.begin_command(kind, workspace, request, out);
    }

    pub(super) fn open_conversation(&mut self, participants: Vec<UserId>, out: &mut Output) {
        let kind = ActionKind::OpenConversation;
        if self.action(kind) == &ActionState::InFlight {
            return;
        }
        let Some(me) = self.me.as_ref().map(|user| user.id) else {
            return;
        };
        if participants.iter().any(|user| self.is_bot(*user)) {
            return self.fail_action(kind, "Apps can't be messaged.", out);
        }
        if participants.iter().any(|user| self.is_deactivated(*user)) {
            let text = "Deactivated people can't be messaged.";
            return self.fail_action(kind, text, out);
        }
        let set = match ParticipantSet::new(me, participants) {
            Ok(set) => set,
            Err(ParticipantError::TooFew) => {
                return self.fail_action(kind, "Choose at least one other person.", out);
            }
            Err(ParticipantError::TooMany) => {
                let message = format!(
                    "A conversation can have at most {} people.",
                    MAX_PARTICIPANTS
                );
                return self.fail_action(kind, message, out);
            }
        };
        let Some(workspace) = self.primary_workspace_id() else {
            return self.fail_action(kind, "Your workspace hasn't loaded yet.", out);
        };
        let request = CommandRequest::Open(OpenConversationRequest {
            operation_id: OperationId::generate(),
            participants: set.users().iter().copied().filter(|u| *u != me).collect(),
        });
        self.begin_command(kind, workspace, request, out);
    }

    /// Starts a create or open. If an earlier attempt for the same thing was given up on with an
    /// unknown outcome, it is continued under the same operation id (the server is asked first),
    /// never repeated blindly with a new one.
    pub(super) fn begin_command(
        &mut self,
        kind: ActionKind,
        workspace: WorkspaceId,
        request: CommandRequest,
        out: &mut Output,
    ) {
        self.set_action(kind, ActionState::InFlight, out);
        let earlier = self
            .commands
            .iter()
            .find(|(_, command)| command.kind == kind)
            .map(|(operation, _)| *operation);
        if let Some(earlier) = earlier {
            let same = self
                .commands
                .get(&earlier)
                .is_some_and(|command| command.request.same_content(&request));
            if same {
                if let Some(command) = self.commands.get_mut(&earlier) {
                    command.stage = CommandStage::Querying;
                    command.resends = 0;
                }
                out.effects
                    .push(Effect::QueryOperationStatus { operation: earlier });
                return;
            }
            self.commands.remove(&earlier);
        }
        let operation = request.operation();
        out.effects.push(request.effect(workspace));
        self.commands.insert(
            operation,
            PendingCommand {
                kind,
                workspace,
                request,
                resends: 0,
                stage: CommandStage::Sending,
            },
        );
    }

    pub(super) fn command_result(
        &mut self,
        operation: OperationId,
        result: Result<lazy_protocol::ConversationResponse, RequestError>,
        out: &mut Output,
    ) {
        let Some(command) = self.commands.get_mut(&operation) else {
            return;
        };
        if command.stage != CommandStage::Sending {
            return;
        }
        let kind = command.kind;
        match result {
            Ok(response) => self.command_succeeded(operation, response.conversation, out),
            Err(error @ RequestError::Rejected(_)) => {
                // Definitively refused: nothing was created, so nothing is left to reconcile.
                self.commands.remove(&operation);
                self.fail_request(kind, &error, out);
            }
            Err(RequestError::Unknown(_)) => {
                command.stage = CommandStage::Querying;
                out.effects.push(Effect::QueryOperationStatus { operation });
            }
        }
    }

    pub(super) fn command_status_result(
        &mut self,
        operation: OperationId,
        result: Result<OperationStatus, RequestError>,
        out: &mut Output,
    ) {
        if !self.commands.contains_key(&operation) {
            return;
        }
        // "It committed" is true whatever the request is doing now (for example when this
        // answer was overtaken by a resend), so it is accepted at any stage.
        if let Ok(OperationStatus::ConversationCommitted { conversation }) = result {
            return self.command_succeeded(operation, conversation, out);
        }
        if let Ok(OperationStatus::GroupCommitted { group }) = result {
            return self.group_created(operation, group, out);
        }
        let Some(command) = self.commands.get_mut(&operation) else {
            return;
        };
        if command.stage != CommandStage::Querying {
            return;
        }
        let kind = command.kind;
        match result {
            Ok(
                OperationStatus::ConversationCommitted { .. }
                | OperationStatus::GroupCommitted { .. },
            ) => {}
            Ok(OperationStatus::NotFound) => {
                if command.resends >= MAX_RESENDS {
                    command.stage = CommandStage::Failed;
                    let message = "Couldn't confirm whether that went through. Try again to check.";
                    return self.fail_action(kind, message, out);
                }
                // The server has no record, and repeating the same operation is idempotent.
                command.resends += 1;
                command.stage = CommandStage::Sending;
                out.effects.push(command.request.effect(command.workspace));
            }
            Ok(OperationStatus::Committed { .. } | OperationStatus::EditCommitted { .. }) => {
                command.stage = CommandStage::Failed;
                self.fail_action(kind, "The server gave an unexpected answer.", out);
            }
            Err(error) if error.is_unauthenticated() => {
                self.commands.remove(&operation);
                self.fail_request(kind, &error, out);
            }
            Err(error) => {
                command.stage = CommandStage::Failed;
                self.fail_request(kind, &error, out);
            }
        }
    }

    pub(super) fn group_command_result(
        &mut self,
        operation: OperationId,
        result: Result<lazy_protocol::GroupResponse, RequestError>,
        out: &mut Output,
    ) {
        let Some(command) = self.commands.get_mut(&operation) else {
            return;
        };
        if command.stage != CommandStage::Sending {
            return;
        }
        let kind = command.kind;
        match result {
            Ok(response) => self.group_created(operation, response.group, out),
            Err(error @ RequestError::Rejected(_)) => {
                self.commands.remove(&operation);
                self.fail_request(kind, &error, out);
            }
            Err(RequestError::Unknown(_)) => {
                command.stage = CommandStage::Querying;
                out.effects.push(Effect::QueryOperationStatus { operation });
            }
        }
    }

    /// The group exists: keep it and finish the action.
    fn group_created(
        &mut self,
        operation: OperationId,
        group: lazy_protocol::UserGroup,
        out: &mut Output,
    ) {
        let Some(command) = self.commands.remove(&operation) else {
            return;
        };
        self.store_group(group, out);
        self.set_action(command.kind, ActionState::Idle, out);
    }

    /// The conversation exists: add it, finish the action, and open it.
    fn command_succeeded(
        &mut self,
        operation: OperationId,
        conversation: Conversation,
        out: &mut Output,
    ) {
        let Some(command) = self.commands.remove(&operation) else {
            return;
        };
        let id = conversation.id;
        self.adopt_conversation(conversation, out);
        self.set_action(command.kind, ActionState::Idle, out);
        self.change_selection(Some(id), out);
        self.mark_failed.remove(&id);
        self.ensure_history(id, out);
        out.changes.push(Change::ConversationOpened(id));
    }
}

// ----- The other management actions -----

impl ClientCore {
    pub(super) fn join_channel(&mut self, id: ConversationId, out: &mut Output) {
        if self.begin_action(ActionKind::Join(id), out) {
            out.effects.push(Effect::JoinChannel { conversation: id });
        }
    }

    pub(super) fn leave_channel(&mut self, id: ConversationId, out: &mut Output) {
        if self.begin_action(ActionKind::Leave(id), out) {
            self.leaving.insert(id);
            out.effects.push(Effect::LeaveChannel { conversation: id });
        }
    }

    pub(super) fn add_members(&mut self, id: ConversationId, users: Vec<UserId>, out: &mut Output) {
        let kind = ActionKind::AddMembers(id);
        if self.action(kind) == &ActionState::InFlight {
            return;
        }
        if users.is_empty() {
            return self.fail_action(kind, "Choose at least one person to add.", out);
        }
        if users.iter().any(|user| self.is_bot(*user)) {
            let text = "Apps join a conversation through their webhook.";
            return self.fail_action(kind, text, out);
        }
        if users.iter().any(|user| self.is_deactivated(*user)) {
            let text = "Deactivated people can't be added to a conversation.";
            return self.fail_action(kind, text, out);
        }
        self.set_action(kind, ActionState::InFlight, out);
        out.effects.push(Effect::AddMembers {
            conversation: id,
            request: AddMembersRequest { user_ids: users },
        });
    }

    pub(super) fn remove_member(&mut self, id: ConversationId, user: UserId, out: &mut Output) {
        if self.begin_action(ActionKind::RemoveMember(id, user), out) {
            out.effects.push(Effect::RemoveMember {
                conversation: id,
                user,
            });
        }
    }

    pub(super) fn update_conversation(
        &mut self,
        id: ConversationId,
        name: Option<String>,
        topic: Option<String>,
        description: Option<String>,
        out: &mut Output,
    ) {
        let kind = ActionKind::UpdateConversation(id);
        if self.action(kind) == &ActionState::InFlight {
            return;
        }
        let name = match name {
            None => None,
            Some(text) => match ChannelName::parse(&normalize_channel_name(&text)) {
                Ok(name) => Some(name),
                Err(error) => return self.fail_action(kind, describe_name_error(&error), out),
            },
        };
        let topic = topic.map(|text| text.trim().to_owned());
        if topic.as_deref().is_some_and(|text| !topic_is_valid(text)) {
            let message = "A topic can be at most 250 characters and can't contain line breaks.";
            return self.fail_action(kind, message, out);
        }
        let description = description.map(|text| text.trim().to_owned());
        if description
            .as_deref()
            .is_some_and(|text| !description_is_valid(text))
        {
            let message = "A description can be at most 1,000 characters.";
            return self.fail_action(kind, message, out);
        }
        self.set_action(kind, ActionState::InFlight, out);
        out.effects.push(Effect::UpdateConversation {
            conversation: id,
            request: UpdateConversationRequest {
                name,
                topic,
                description,
            },
        });
    }

    pub(super) fn archive_conversation(
        &mut self,
        id: ConversationId,
        archive: bool,
        out: &mut Output,
    ) {
        let (kind, effect) = if archive {
            (
                ActionKind::Archive(id),
                Effect::ArchiveConversation { conversation: id },
            )
        } else {
            (
                ActionKind::Unarchive(id),
                Effect::UnarchiveConversation { conversation: id },
            )
        };
        if self.begin_action(kind, out) {
            out.effects.push(effect);
        }
    }

    pub(super) fn set_favorite(&mut self, id: ConversationId, favorite: bool, out: &mut Output) {
        if self.begin_action(ActionKind::SetFavorite(id), out) {
            out.effects.push(Effect::SetFavorite {
                conversation: id,
                favorite,
            });
        }
    }

    /// Applies the result of any management request except create and open.
    pub(super) fn action_result(
        &mut self,
        kind: ActionKind,
        result: Result<ActionOutcome, RequestError>,
        out: &mut Output,
    ) {
        // A result for an action that is not in flight is a stale duplicate.
        if self.action(kind) != &ActionState::InFlight {
            return;
        }
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                if let ActionKind::Leave(id) = kind {
                    self.leaving.remove(&id);
                }
                // Creating a workspace repeats the same request after an unknown outcome.
                if kind == ActionKind::CreateWorkspace
                    && matches!(error, RequestError::Unknown(_))
                    && self.retry_create_workspace(out)
                {
                    return;
                }
                if matches!(&error, RequestError::Rejected(api) if api.code == ErrorCode::NotFound)
                    && self.group_already_gone(kind, out)
                {
                    return;
                }
                // A webhook that is already gone is what a delete wanted.
                if let ActionKind::DeleteWebhook(webhook) = kind
                    && matches!(&error, RequestError::Rejected(api) if api.code == ErrorCode::NotFound)
                {
                    self.webhook_deleted(webhook, out);
                    return self.set_action(kind, ActionState::Idle, out);
                }
                // An emoji that is already gone is what a delete wanted.
                if let ActionKind::DeleteCustomEmoji(emoji) = kind
                    && matches!(&error, RequestError::Rejected(api) if api.code == ErrorCode::NotFound)
                {
                    self.mark_emoji_deleted(emoji, out);
                    return self.set_action(kind, ActionState::Idle, out);
                }
                // Whether a webhook was created is unknown when the answer is lost, and the
                // address cannot be shown again: show the list, where it can be deleted.
                let lost_create = matches!(
                    (kind, &error),
                    (ActionKind::CreateWebhook(_), RequestError::Unknown(_))
                );
                self.fail_request(kind, &error, out);
                if let (true, ActionKind::CreateWebhook(conversation)) = (lost_create, kind) {
                    self.load_webhooks(conversation, out);
                }
                // The same for an invitation: its code cannot be shown again, so the list shows
                // whether it was made.
                if matches!(
                    (kind, &error),
                    (ActionKind::CreateInvitation, RequestError::Unknown(_))
                ) {
                    self.invitation_create_lost(out);
                }
                // A revocation whose answer was lost may have happened: the list shows which.
                if matches!(
                    (kind, &error),
                    (ActionKind::RevokeInvitation(_), RequestError::Unknown(_))
                ) {
                    self.reload_invitations(out);
                }
                return;
            }
        };
        match (kind, outcome) {
            (ActionKind::Join(id), ActionOutcome::Conversation(conversation)) => {
                let is_new = self.adopt_conversation(conversation, out);
                self.set_browse_joined(id, true, out);
                if is_new {
                    self.request_history(id, None, out);
                }
            }
            (ActionKind::Leave(id), _) => {
                self.remove_conversation(id, false, out);
                self.leaving.remove(&id);
            }
            (
                ActionKind::AddMembers(_)
                | ActionKind::UpdateConversation(_)
                | ActionKind::Archive(_)
                | ActionKind::Unarchive(_),
                ActionOutcome::Conversation(conversation),
            ) => {
                self.replace_conversation(conversation, out);
            }
            (ActionKind::SetFavorite(id), ActionOutcome::UserState(state)) => {
                if let Some(current) = self.user_states.get_mut(&id) {
                    current.favorite = state.favorite;
                    out.changes
                        .push(Change::ConversationStateChanged { conversation: id });
                    self.cache_changed(true);
                }
            }
            // Removing someone changes the member count through the event that follows.
            (ActionKind::RemoveMember(..), _) => {}
            (
                ActionKind::CreateGroup | ActionKind::UpdateGroup(_) | ActionKind::DeleteGroup(_),
                outcome,
            ) => self.group_action_result(kind, outcome, out),
            (
                ActionKind::CreateWebhook(_)
                | ActionKind::DeleteWebhook(_)
                | ActionKind::LoadWebhooks(_),
                outcome,
            ) => self.webhook_action_result(kind, outcome, out),
            (ActionKind::RenameCustomEmoji(_) | ActionKind::DeleteCustomEmoji(_), outcome) => {
                self.emoji_action_result(kind, outcome, out)
            }
            (
                ActionKind::UpdateWorkspace
                | ActionKind::SetMemberRole(_)
                | ActionKind::SetMemberDeactivated(_)
                | ActionKind::CreateInvitation
                | ActionKind::RevokeInvitation(_),
                outcome,
            ) => self.admin_action_result(kind, outcome, out),
            (
                ActionKind::CreateWorkspace
                | ActionKind::AcceptInvitation
                | ActionKind::UpdateProfile
                | ActionKind::SetAvatar,
                outcome,
            ) => {
                self.workspace_action_result(kind, outcome, out);
            }
            // A response that does not match the request is ignored.
            _ => {}
        }
        self.set_action(kind, ActionState::Idle, out);
    }
}

// ----- Helpers -----

pub(super) fn default_user_state(conversation: ConversationId) -> ConversationUserState {
    ConversationUserState {
        conversation_id: conversation,
        favorite: false,
        last_read: EventSeq::ZERO,
        unread_count: 0,
        mention_count: 0,
    }
}

fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Turns what a person types into a channel name: surrounding spaces and a leading `#` are
/// dropped, letters are lowercased, and spaces become hyphens.
fn normalize_channel_name(input: &str) -> String {
    input
        .trim()
        .trim_start_matches('#')
        .to_lowercase()
        .replace(' ', "-")
}

fn describe_name_error(error: &NameError) -> String {
    match error {
        NameError::TooShort { .. } => "Give the channel a name.".to_owned(),
        NameError::TooLong { max, .. } => {
            format!("A channel name can be at most {max} characters.")
        }
        NameError::InvalidCharacter { .. } => {
            "A channel name can only use letters, numbers, hyphens, and underscores.".to_owned()
        }
        NameError::InvalidStart { .. } => {
            "A channel name must start with a letter or number.".to_owned()
        }
    }
}

/// Words for a person about why a request failed.
pub(super) fn readable_error(error: &RequestError) -> String {
    match error {
        RequestError::Unknown(_) => {
            "Couldn't reach the server. Check your connection and try again.".to_owned()
        }
        RequestError::Rejected(api) => match api.code {
            ErrorCode::NameTaken => "That name is already taken. Choose another.".to_owned(),
            ErrorCode::Forbidden => "You're not allowed to do that.".to_owned(),
            ErrorCode::NotFound => {
                "That conversation no longer exists, or you can't see it.".to_owned()
            }
            ErrorCode::Conflict => "That clashes with a change that was already made.".to_owned(),
            ErrorCode::StaleRevision => {
                "That message changed while you were editing it.".to_owned()
            }
            ErrorCode::RateLimited => "Too many requests. Wait a moment and try again.".to_owned(),
            ErrorCode::PayloadTooLarge => "That's too large to send.".to_owned(),
            ErrorCode::UnsupportedMedia => "That kind of file can't be shared.".to_owned(),
            ErrorCode::LimitReached => "That's more than can be held.".to_owned(),
            ErrorCode::Unauthenticated => "Your session has expired. Sign in again.".to_owned(),
            ErrorCode::InvitationInvalid => {
                "That invitation isn't valid. It may have expired, been used up, or been \
                 revoked."
                    .to_owned()
            }
            ErrorCode::InvalidRequest
            | ErrorCode::InvalidCredentials
            | ErrorCode::UsernameTaken
            | ErrorCode::Internal
            | ErrorCode::Unavailable => {
                if api.message.is_empty() {
                    "Something went wrong. Try again.".to_owned()
                } else {
                    api.message.clone()
                }
            }
        },
    }
}
