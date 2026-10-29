//! User groups, and who is in each conversation.
//!
//! # Groups
//!
//! The groups of a workspace arrive in the bootstrap, are kept current by `GroupChanged` (which
//! replaces the group) and `GroupDeleted`, and are saved in the content cache. Only admins and
//! owners manage them (`ClientCore::can_manage_groups`); the server decides. Creating a group
//! carries an operation id and is reconciled like creating a channel (`GroupCommitted` or
//! `not_found`). Changing a group sets its handle, name, and complete membership, and deleting is
//! idempotent, so those are plain requests.
//!
//! # Who is in a conversation
//!
//! Direct and group conversations list their participants. For a channel the people are fetched
//! (`GET /v1/conversations/{id}/members`) when it is selected, and again after the realtime
//! connection catches up, and kept current by `MemberAdded` and `MemberRemoved`. A change that
//! arrives while a fetch is in flight is applied to the answer, so the snapshot cannot undo it.
//! The lists are not saved: they are a convenience for the composer.

use lazy_domain::{
    DisplayName, GroupHandle, GroupId, MAX_GROUP_MEMBERS, MAX_GROUPS_PER_WORKSPACE, NameError,
};
use lazy_protocol::{
    ConversationMembersResponse, CreateGroupRequest, ErrorCode, UpdateGroupRequest, UserGroup,
};

use super::conversations::{CommandRequest, readable_error};
use super::*;
use crate::ActionOutcome;
use lazy_domain::WorkspaceRole;

/// Who is in one channel, as far as the client knows.
#[derive(Debug, Default)]
pub(super) struct MemberList {
    users: Vec<UserId>,
    /// An answer has been applied, so `users` is real.
    loaded: bool,
    fetching: bool,
    /// Identifies the request in flight, so an answer to an earlier one (from before the lists
    /// were dropped) is not taken for the answer to this one.
    generation: u64,
    /// Loaded before the connection last caught up; fetch again when it is looked at.
    stale: bool,
    /// The last fetch failed; tried again when the person selects it or the connection catches up.
    failed: bool,
    /// Membership changes that arrived while a fetch was in flight, oldest first. `true` is
    /// added, `false` removed.
    buffered: Vec<(UserId, bool)>,
}

// ----- Read model -----

impl ClientCore {
    /// The workspace's user groups, ordered by handle.
    pub fn groups(&self) -> &[UserGroup] {
        &self.groups
    }

    /// One group, by id (for showing a mention of it).
    pub fn group(&self, id: GroupId) -> Option<&UserGroup> {
        self.groups.iter().find(|group| group.id == id)
    }

    /// Whether to offer creating, changing, and deleting groups: admins and owners of the
    /// workspace. The server decides.
    pub fn can_manage_groups(&self) -> bool {
        self.primary_workspace()
            .is_some_and(|info| info.role >= WorkspaceRole::Admin)
    }

    /// Who is in a conversation: the participants of a direct or group conversation, or for a
    /// channel the people fetched when it was selected and kept current since. `None` while a
    /// channel's list has not arrived, and for a conversation the person is not in.
    pub fn conversation_members(&self, conversation: ConversationId) -> Option<&[UserId]> {
        let conv = self.conversation(conversation)?;
        match conv.kind {
            lazy_domain::ConversationKind::Direct | lazy_domain::ConversationKind::Group => {
                Some(&conv.participants)
            }
            lazy_domain::ConversationKind::Channel => self
                .member_lists
                .get(&conversation)
                .filter(|list| list.loaded)
                .map(|list| list.users.as_slice()),
        }
    }
}

// ----- Fetching members -----

impl ClientCore {
    /// Asks for the selected channel's people if they are not known, are out of date, or the last
    /// attempt failed and something has changed. Runs after every input.
    pub(super) fn ensure_selected_members(&mut self, out: &mut Output) {
        // After a refused cursor everything is fetched again; wait for the new bootstrap.
        if !matches!(self.phase, SessionPhase::Ready) || self.cache_suppressed {
            return;
        }
        let Some(selected) = self.selected else {
            return;
        };
        self.start_member_fetch(selected, out);
    }

    /// Asks for a channel's people unless they are known and current, or a request is in flight
    /// or has just failed.
    fn start_member_fetch(&mut self, conversation: ConversationId, out: &mut Output) {
        let Some(conv) = self.conversation(conversation) else {
            return;
        };
        if conv.kind != lazy_domain::ConversationKind::Channel {
            return;
        }
        let list = self.member_lists.entry(conversation).or_default();
        if list.fetching || list.failed || (list.loaded && !list.stale) {
            return;
        }
        self.next_members_fetch += 1;
        list.fetching = true;
        list.generation = self.next_members_fetch;
        list.buffered.clear();
        out.effects.push(Effect::FetchMembers {
            conversation,
            generation: list.generation,
        });
    }

    /// Whether text that may mention someone has to wait for the channel's people: it contains an
    /// `@`, the channel's list has not arrived, and asking for it has not failed. Sent now, a
    /// name would silently become plain text because the directory cannot know who is in the
    /// channel. Starts the fetch if none is under way.
    pub(super) fn must_wait_for_members(
        &mut self,
        conversation: ConversationId,
        text: &str,
        out: &mut Output,
    ) -> bool {
        if !text.contains('@')
            || !matches!(self.phase, SessionPhase::Ready)
            || self.conversation(conversation).is_none()
            || self.conversation_members(conversation).is_some()
        {
            return false;
        }
        if self
            .conversation(conversation)
            .is_some_and(|c| c.kind != lazy_domain::ConversationKind::Channel)
        {
            return false;
        }
        if self
            .member_lists
            .get(&conversation)
            .is_some_and(|list| list.failed)
        {
            return false;
        }
        self.start_member_fetch(conversation, out);
        true
    }

    /// Whether anything is held for `conversation`: text written after it goes behind it, even
    /// text that mentions nobody, so messages keep the order they were written in.
    pub(super) fn has_held_text(&self, conversation: ConversationId) -> bool {
        self.held_text.iter().any(|held| match held {
            Input::SendMessage {
                conversation: c, ..
            }
            | Input::SendReply {
                conversation: c, ..
            }
            | Input::EditMessage {
                conversation: c, ..
            } => *c == conversation,
            _ => false,
        })
    }

    /// Sends, replies, and edits that were held for a channel's people go as soon as the list
    /// has arrived (or asking for it has failed), in the order they were made.
    pub(super) fn release_held_text(&mut self, out: &mut Output) {
        if self.held_text.is_empty() {
            return;
        }
        let held = std::mem::take(&mut self.held_text);
        for input in held {
            let (conversation, text) = match &input {
                Input::SendMessage { conversation, text }
                | Input::SendReply {
                    conversation, text, ..
                }
                | Input::EditMessage {
                    conversation, text, ..
                } => (*conversation, text.clone()),
                _ => continue,
            };
            // Anything held for the same conversation earlier keeps its place: dispatching holds
            // a text again while an earlier one is held (see `has_held_text`).
            if self.must_wait_for_members(conversation, &text, out) {
                self.held_text.push(input);
                continue;
            }
            let released = self.dispatch(input);
            out.effects.extend(released.effects);
            out.changes.extend(released.changes);
        }
    }

    /// The person chose a conversation: a failed fetch is worth trying again.
    pub(super) fn retry_member_fetch(&mut self, conversation: ConversationId) {
        if let Some(list) = self.member_lists.get_mut(&conversation) {
            list.failed = false;
        }
    }

    /// The connection caught up: every list may have missed something, and a failed fetch may
    /// work now.
    pub(super) fn members_may_be_stale(&mut self) {
        for list in self.member_lists.values_mut() {
            list.stale = list.loaded;
            list.failed = false;
        }
    }

    pub(super) fn members_fetched(
        &mut self,
        conversation: ConversationId,
        generation: u64,
        result: Result<ConversationMembersResponse, RequestError>,
        out: &mut Output,
    ) {
        let Some(list) = self.member_lists.get_mut(&conversation) else {
            return;
        };
        if !list.fetching || list.generation != generation {
            return;
        }
        list.fetching = false;
        match result {
            Ok(response) if response.conversation_id == conversation => {
                list.users = response.user_ids;
                for (user, added) in std::mem::take(&mut list.buffered) {
                    apply_member_change(&mut list.users, user, added);
                }
                list.loaded = true;
                list.stale = false;
                list.failed = false;
                out.changes
                    .push(Change::ConversationMembersChanged { conversation });
            }
            Ok(_) => list.failed = true,
            Err(error) if error.is_unauthenticated() => self.session_expired(out),
            Err(_) => list.failed = true,
        }
    }

    /// Someone was added to or removed from a channel.
    pub(super) fn member_list_changed(
        &mut self,
        conversation: ConversationId,
        user: UserId,
        added: bool,
        out: &mut Output,
    ) {
        let Some(list) = self.member_lists.get_mut(&conversation) else {
            return;
        };
        if list.fetching {
            list.buffered.push((user, added));
        }
        if list.loaded && apply_member_change(&mut list.users, user, added) {
            out.changes
                .push(Change::ConversationMembersChanged { conversation });
        }
    }
}

/// Adds or removes `user`. Returns whether the list changed.
fn apply_member_change(users: &mut Vec<UserId>, user: UserId, added: bool) -> bool {
    match (users.iter().position(|member| *member == user), added) {
        (None, true) => {
            users.push(user);
            true
        }
        (Some(at), false) => {
            users.remove(at);
            true
        }
        _ => false,
    }
}

// ----- Events about groups -----

impl ClientCore {
    fn workspace_is_known(&self, workspace: WorkspaceId) -> bool {
        self.workspaces
            .iter()
            .any(|info| info.workspace.id == workspace)
    }

    /// Takes a group from the server (created, changed, or an answer), replacing what is held.
    pub(super) fn store_group(&mut self, group: UserGroup, out: &mut Output) {
        if !self.workspace_is_known(group.workspace_id) {
            return;
        }
        match self.groups.iter_mut().find(|held| held.id == group.id) {
            Some(held) => *held = group,
            None => self.groups.push(group),
        }
        self.groups.sort_by(|a, b| {
            a.handle
                .as_str()
                .cmp(b.handle.as_str())
                .then(a.id.cmp(&b.id))
        });
        out.changes.push(Change::GroupsChanged);
        self.cache_changed(true);
    }

    pub(super) fn apply_group_changed(&mut self, group: UserGroup, out: &mut Output) {
        self.store_group(group, out);
    }

    pub(super) fn apply_group_deleted(&mut self, id: GroupId, out: &mut Output) {
        let before = self.groups.len();
        self.groups.retain(|group| group.id != id);
        if self.groups.len() != before {
            out.changes.push(Change::GroupsChanged);
            self.cache_changed(true);
        }
    }

    /// Replaces the groups with those of a bootstrap.
    pub(super) fn adopt_workspace_groups(&mut self, workspace: &lazy_protocol::WorkspaceBootstrap) {
        self.groups = workspace.groups.clone();
        self.groups.sort_by(|a, b| {
            a.handle
                .as_str()
                .cmp(b.handle.as_str())
                .then(a.id.cmp(&b.id))
        });
    }
}

// ----- Actions -----

/// Words for a person about why a group request failed.
pub(super) fn group_error(error: &RequestError) -> String {
    match error {
        RequestError::Rejected(api) => match api.code {
            ErrorCode::NameTaken => {
                "That handle is already used by a person, another group, or a reserved word. \
                 Choose another."
                    .to_owned()
            }
            ErrorCode::LimitReached => format!(
                "A workspace can have at most {MAX_GROUPS_PER_WORKSPACE} groups, with up to \
                 {MAX_GROUP_MEMBERS} people each."
            ),
            ErrorCode::Forbidden => "Only admins and owners can manage groups.".to_owned(),
            ErrorCode::NotFound => "That group no longer exists.".to_owned(),
            _ => readable_error(error),
        },
        RequestError::Unknown(_) => readable_error(error),
    }
}

fn describe_handle_error(error: &NameError) -> String {
    match error {
        NameError::TooShort { .. } => "A group handle needs at least 2 characters.".to_owned(),
        NameError::TooLong { max, .. } => {
            format!("A group handle can be at most {max} characters.")
        }
        _ => "A group handle can use lowercase letters, digits, dots, underscores, and hyphens, \
              must start and end with a letter or digit, and can't be channel, here, or everyone."
            .to_owned(),
    }
}

fn describe_group_name_error(error: &NameError) -> String {
    match error {
        NameError::TooShort { .. } => "Give the group a name.".to_owned(),
        NameError::TooLong { max, .. } => format!("A group name can be at most {max} characters."),
        _ => "A group name can't contain control characters.".to_owned(),
    }
}

/// A handle as typed: with or without the `@`, any case.
fn parse_group_handle(typed: &str) -> Result<GroupHandle, String> {
    GroupHandle::parse(typed.trim().trim_start_matches('@'))
        .map_err(|error| describe_handle_error(&error))
}

fn parse_group_name(typed: &str) -> Result<DisplayName, String> {
    DisplayName::parse(typed.trim()).map_err(|error| describe_group_name_error(&error))
}

/// `members` without repeats, in the order given.
fn distinct(members: Vec<UserId>) -> Vec<UserId> {
    let mut seen = HashSet::new();
    members.into_iter().filter(|id| seen.insert(*id)).collect()
}

impl ClientCore {
    /// Checks what can be checked before asking: the person's role, and the size of the group.
    /// A deactivated person can stay in a group they are in, but no one is added who is
    /// deactivated.
    fn check_group_request(
        &mut self,
        kind: ActionKind,
        group: Option<GroupId>,
        members: Option<&Vec<UserId>>,
        out: &mut Output,
    ) -> bool {
        if !self.can_manage_groups() {
            self.fail_action(kind, "Only admins and owners can manage groups.", out);
            return false;
        }
        if members.is_some_and(|members| members.iter().any(|user| self.is_bot(*user))) {
            self.fail_action(kind, "Apps can't be members of a group.", out);
            return false;
        }
        let current: &[UserId] = group
            .and_then(|id| self.groups.iter().find(|held| held.id == id))
            .map_or(&[], |held| held.members.as_slice());
        if members.is_some_and(|members| {
            members
                .iter()
                .any(|user| self.is_deactivated(*user) && !current.contains(user))
        }) {
            self.fail_action(kind, "Deactivated people can't be added to a group.", out);
            return false;
        }
        if members.is_some_and(|members| members.len() > MAX_GROUP_MEMBERS) {
            let message = format!("A group can have at most {MAX_GROUP_MEMBERS} people.");
            self.fail_action(kind, message, out);
            return false;
        }
        true
    }

    /// Whether a handle belongs to a person, a group other than `except`, or a reserved word.
    fn handle_is_in_use(&self, handle: &GroupHandle, except: Option<GroupId>) -> bool {
        let handle = handle.as_str();
        self.groups
            .iter()
            .any(|group| Some(group.id) != except && group.handle.as_str() == handle)
            || self
                .users
                .values()
                .any(|user| user.username.as_str().eq_ignore_ascii_case(handle))
    }

    pub(super) fn create_group(
        &mut self,
        handle: &str,
        name: &str,
        members: Vec<UserId>,
        out: &mut Output,
    ) {
        let kind = ActionKind::CreateGroup;
        if self.action(kind) == &ActionState::InFlight {
            return;
        }
        let members = distinct(members);
        if !self.check_group_request(kind, None, Some(&members), out) {
            return;
        }
        let handle = match parse_group_handle(handle) {
            Ok(handle) => handle,
            Err(message) => return self.fail_action(kind, message, out),
        };
        let name = match parse_group_name(name) {
            Ok(name) => name,
            Err(message) => return self.fail_action(kind, message, out),
        };
        let Some(workspace) = self.primary_workspace_id() else {
            return self.fail_action(kind, "Your workspace hasn't loaded yet.", out);
        };
        if self.handle_is_in_use(&handle, None) {
            return self.fail_action(kind, group_error(&name_taken()), out);
        }
        let in_workspace = self
            .groups
            .iter()
            .filter(|group| group.workspace_id == workspace)
            .count();
        if in_workspace >= MAX_GROUPS_PER_WORKSPACE {
            return self.fail_action(kind, group_error(&limit_reached()), out);
        }
        let request = CommandRequest::Group(CreateGroupRequest {
            operation_id: OperationId::generate(),
            handle,
            name,
            members,
        });
        self.begin_command(kind, workspace, request, out);
    }

    pub(super) fn update_group(
        &mut self,
        id: GroupId,
        handle: Option<String>,
        name: Option<String>,
        members: Option<Vec<UserId>>,
        out: &mut Output,
    ) {
        let kind = ActionKind::UpdateGroup(id);
        if self.action(kind) == &ActionState::InFlight {
            return;
        }
        let members = members.map(distinct);
        if !self.check_group_request(kind, Some(id), members.as_ref(), out) {
            return;
        }
        if !self.groups.iter().any(|group| group.id == id) {
            return self.fail_action(kind, "That group no longer exists.", out);
        }
        let handle = match handle.as_deref().map(parse_group_handle).transpose() {
            Ok(handle) => handle,
            Err(message) => return self.fail_action(kind, message, out),
        };
        if handle
            .as_ref()
            .is_some_and(|handle| self.handle_is_in_use(handle, Some(id)))
        {
            return self.fail_action(kind, group_error(&name_taken()), out);
        }
        let name = match name.as_deref().map(parse_group_name).transpose() {
            Ok(name) => name,
            Err(message) => return self.fail_action(kind, message, out),
        };
        if handle.is_none() && name.is_none() && members.is_none() {
            return self.fail_action(kind, "There is nothing to change.", out);
        }
        self.set_action(kind, ActionState::InFlight, out);
        out.effects.push(Effect::UpdateGroup {
            group: id,
            request: UpdateGroupRequest {
                handle,
                name,
                members,
            },
        });
    }

    pub(super) fn delete_group(&mut self, id: GroupId, out: &mut Output) {
        let kind = ActionKind::DeleteGroup(id);
        if !self.check_group_request(kind, Some(id), None, out) {
            return;
        }
        if self.begin_action(kind, out) {
            out.effects.push(Effect::DeleteGroup { group: id });
        }
    }

    /// The answer to a group request that is not a creation.
    pub(super) fn group_action_result(
        &mut self,
        kind: ActionKind,
        outcome: ActionOutcome,
        out: &mut Output,
    ) {
        match (kind, outcome) {
            (ActionKind::UpdateGroup(_), ActionOutcome::Group(group)) => {
                self.store_group(group, out);
            }
            (ActionKind::DeleteGroup(id), _) => self.apply_group_deleted(id, out),
            _ => {}
        }
    }

    /// Deleting a group that is already gone is deleting it.
    pub(super) fn group_already_gone(&mut self, kind: ActionKind, out: &mut Output) -> bool {
        if let ActionKind::DeleteGroup(id) = kind {
            self.apply_group_deleted(id, out);
            self.set_action(kind, ActionState::Idle, out);
            return true;
        }
        false
    }
}

fn name_taken() -> RequestError {
    RequestError::Rejected(lazy_protocol::ApiError::new(ErrorCode::NameTaken, ""))
}

fn limit_reached() -> RequestError {
    RequestError::Rejected(lazy_protocol::ApiError::new(ErrorCode::LimitReached, ""))
}
