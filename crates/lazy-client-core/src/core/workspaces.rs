//! Several workspaces, the person's profile and avatar, and the appearance of this device (slice
//! 3G, D23).
//!
//! **Scopes.** Everything that belongs to one workspace (its people and their statuses there, its
//! conversations, groups, custom emoji, the person's notification settings and status, the views
//! of the thread inbox, search, saved messages, directory, and invitations) lives in the core's
//! ordinary fields for the *loaded scope*; the other workspaces' copies are parked in
//! `parked`. Switching, and applying an event or an answer that belongs to a workspace in the
//! background, exchange the scopes around the ordinary code, so that code is the same whichever
//! workspace it works for. Inside such a window the loaded scope is not the active workspace;
//! `handle` always ends with them equal. What is shared by all workspaces (the signed-in person,
//! timelines, read states and threads keyed by id, drafts, the connections, the cache, the
//! notice, preferences for conversations, saved marks, recent searches and visits, and the
//! counters that number requests) is never exchanged.

use std::collections::{HashMap, HashSet, VecDeque};

use lazy_domain::workspace::{MAX_AVATAR_UPLOAD_BYTES, invite_code_is_wellformed};
use lazy_domain::{
    ConversationId, ConversationKind, DisplayName, UserId, WorkspaceId, WorkspaceName,
};
use lazy_protocol::{
    AcceptInvitationRequest, Conversation, CreateWorkspaceRequest, InviteCode,
    UpdateProfileRequest, User, UserGroup, WorkspaceBootstrap,
};

use crate::attachments::{FileSource, ImageData, ImageState, TransferError};
use crate::change::{Change, Output};
use crate::effect::Effect;
use crate::input::Input;
use crate::model::{
    ActionKind, ActionOutcome, ActionState, ConnectionState, RequestError, SessionPhase,
    WorkspaceInfo,
};
use crate::workspaces::{
    Appearance, MAX_HELD_AVATAR_BYTES, MAX_HELD_AVATARS, MAX_QUEUED_AVATAR_FETCHES, RailItem,
};

use super::ClientCore;
use super::admin::Administration;
use super::admin::MALFORMED_INVITE_CODE;
use super::discovery::Discovery;
use super::emoji::Emojis;
use super::inbox::Inboxing;
use super::notify::Notifications;
use super::search::Searching;

/// How many times creating a workspace is sent when the outcome of a try is unknown.
const MAX_CREATE_ATTEMPTS: u32 = 3;

/// What belongs to one workspace, parked while another is loaded.
#[derive(Debug, Default)]
pub(super) struct WorkspaceScope {
    users: HashMap<UserId, User>,
    conversations: Vec<Conversation>,
    groups: Vec<UserGroup>,
    emojis: Emojis,
    searching: Searching,
    discovery: Discovery,
    notif: Notifications,
    inboxing: Inboxing,
    admin: Administration,
}

/// Creating a workspace: its id is chosen once, so every try is the same request.
#[derive(Debug)]
pub(super) struct CreateJob {
    workspace: WorkspaceId,
    name: WorkspaceName,
    attempts: u32,
}

/// What the core keeps about the avatars of people.
#[derive(Debug, Default)]
pub(super) struct Avatars {
    /// For each person asked about: the version shown and its state.
    images: HashMap<UserId, (u32, ImageState)>,
    /// People whose image is ready, least recently used first.
    order: VecDeque<UserId>,
    bytes: usize,
    queue: VecDeque<UserId>,
    in_flight: HashSet<(UserId, u32)>,
}

// ----- Scopes -----

impl ClientCore {
    fn swap_scope(&mut self, parked: &mut WorkspaceScope) {
        std::mem::swap(&mut self.users, &mut parked.users);
        std::mem::swap(&mut self.conversations, &mut parked.conversations);
        std::mem::swap(&mut self.groups, &mut parked.groups);
        std::mem::swap(&mut self.emojis, &mut parked.emojis);
        self.searching.swap_scoped(&mut parked.searching);
        self.discovery.swap_scoped(&mut parked.discovery);
        self.notif.swap_scoped(&mut parked.notif);
        self.inboxing.swap_scoped(&mut parked.inboxing);
        self.admin.swap_scoped(&mut parked.admin);
    }

    /// Loads another workspace's scope, parking the one that was loaded.
    pub(super) fn enter_scope(&mut self, workspace: Option<WorkspaceId>) {
        if self.scope == workspace {
            return;
        }
        let mut incoming = workspace
            .and_then(|id| self.parked.remove(&id))
            .unwrap_or_default();
        self.swap_scope(&mut incoming);
        if let Some(previous) = self.scope {
            self.parked.insert(previous, incoming);
        }
        self.scope = workspace;
    }

    /// Runs `work` with `workspace`'s scope loaded, then loads the one that was loaded before.
    pub(super) fn in_scope<R>(
        &mut self,
        workspace: WorkspaceId,
        work: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let back = self.scope;
        self.enter_scope(Some(workspace));
        let result = work(self);
        self.enter_scope(back);
        result
    }

    pub(super) fn knows_workspace(&self, workspace: WorkspaceId) -> bool {
        self.workspaces
            .iter()
            .any(|info| info.workspace.id == workspace)
    }

    /// The workspace an input belongs to, when it is not the loaded one's: events of a workspace
    /// in the background, and the answers to requests made for one.
    pub(super) fn route_scope(&self, input: &Input) -> Option<WorkspaceId> {
        use lazy_protocol::ServerFrame;
        let workspace = match input {
            Input::Frame(ServerFrame::Event(envelope)) => Some(envelope.workspace_id),
            Input::ActionResult { kind, .. } => match kind {
                ActionKind::CreateWorkspace
                | ActionKind::AcceptInvitation
                | ActionKind::UpdateProfile
                | ActionKind::SetAvatar => None,
                _ => self.action_scope.get(kind).copied(),
            },
            Input::NotificationSettingsResult { workspace, .. }
            | Input::StatusResult { workspace, .. } => Some(*workspace),
            Input::EmojiImageLoaded { emoji, .. } | Input::CustomEmojiUploaded { emoji, .. } => {
                self.workspace_of_emoji(*emoji)
            }
            Input::GroupCommandResult { operation, .. }
            | Input::ConversationCommandResult { operation, .. } => self
                .commands
                .get(operation)
                .map(|command| command.workspace()),
            _ => None,
        }?;
        (Some(workspace) != self.scope && self.knows_workspace(workspace)).then_some(workspace)
    }

    fn workspace_of_emoji(&self, emoji: lazy_domain::EmojiId) -> Option<WorkspaceId> {
        let holds = |emojis: &Emojis| emojis.holds(emoji);
        if self.scope.is_some() && holds(&self.emojis) {
            return self.scope;
        }
        self.parked
            .iter()
            .find(|(_, scope)| holds(&scope.emojis))
            .map(|(id, _)| *id)
    }

    /// The conversations of a workspace, whether or not it is the loaded one.
    pub(super) fn scope_conversations(&self, workspace: WorkspaceId) -> &[Conversation] {
        if self.scope == Some(workspace) {
            &self.conversations
        } else {
            self.parked
                .get(&workspace)
                .map_or(&[], |scope| scope.conversations.as_slice())
        }
    }

    /// The people of a workspace as it knows them, whether or not it is the loaded one.
    pub(super) fn scope_users(&self, workspace: WorkspaceId) -> Option<&HashMap<UserId, User>> {
        if self.scope == Some(workspace) {
            Some(&self.users)
        } else {
            self.parked.get(&workspace).map(|scope| &scope.users)
        }
    }

    pub(super) fn scope_groups(&self, workspace: WorkspaceId) -> &[UserGroup] {
        if self.scope == Some(workspace) {
            &self.groups
        } else {
            self.parked
                .get(&workspace)
                .map_or(&[], |scope| scope.groups.as_slice())
        }
    }

    pub(super) fn scope_emoji(&self, workspace: WorkspaceId) -> &[lazy_protocol::CustomEmoji] {
        if self.scope == Some(workspace) {
            self.emojis.list_slice()
        } else {
            self.parked
                .get(&workspace)
                .map_or(&[], |scope| scope.emojis.list_slice())
        }
    }

    /// A conversation of any workspace.
    pub(super) fn find_conversation(&self, id: ConversationId) -> Option<&Conversation> {
        self.conversations.iter().find(|c| c.id == id).or_else(|| {
            self.parked
                .values()
                .find_map(|scope| scope.conversations.iter().find(|c| c.id == id))
        })
    }

    /// A person as any workspace knows them (the loaded one first).
    pub(super) fn find_user(&self, id: UserId) -> Option<&User> {
        self.users
            .get(&id)
            .or_else(|| self.parked.values().find_map(|scope| scope.users.get(&id)))
    }

    /// Every conversation of every workspace.
    pub(super) fn all_conversation_ids(&self) -> HashSet<ConversationId> {
        self.conversations
            .iter()
            .chain(
                self.parked
                    .values()
                    .flat_map(|scope| scope.conversations.iter()),
            )
            .map(|c| c.id)
            .collect()
    }
}

// ----- Reading -----

impl ClientCore {
    /// One entry per workspace the person belongs to, in the order the server lists them.
    pub fn workspace_rail(&self) -> Vec<RailItem> {
        self.workspaces
            .iter()
            .map(|info| {
                let id = info.workspace.id;
                let conversations = self.scope_conversations(id);
                let attention = self.attention_of(conversations);
                RailItem {
                    id,
                    name: info.workspace.name.clone(),
                    initials: initials(&info.workspace.name),
                    role: info.role,
                    active: Some(id) == self.active,
                    attention,
                    unread: attention == 0 && self.unread_of(conversations),
                    connection: self
                        .sockets
                        .get(&id)
                        .copied()
                        .unwrap_or(ConnectionState::Offline),
                }
            })
            .collect()
    }

    /// The workspace the screen shows (`primary_workspace()` is its entry).
    pub fn active_workspace(&self) -> Option<WorkspaceId> {
        self.active
    }

    /// What needs the person's attention in every workspace together: the number for the window
    /// title and the launcher badge.
    pub fn attention_total(&self) -> u32 {
        self.workspaces.iter().fold(0u32, |total, info| {
            total.saturating_add(self.attention_of(self.scope_conversations(info.workspace.id)))
        })
    }

    /// How this device looks and moves.
    pub fn appearance(&self) -> Appearance {
        self.appearance
    }

    /// A person's picture, once asked for with `Input::LoadAvatar`.
    pub fn avatar(&self, user: UserId) -> Option<&ImageState> {
        self.avatars.images.get(&user).map(|(_, state)| state)
    }

    /// Keeps the rail's numbers: a `RailChanged` follows whenever any of them moves.
    pub(super) fn sync_rail(&mut self, out: &mut Output) {
        let rail: Vec<(WorkspaceId, String, u32, bool, ConnectionState, bool)> = self
            .workspace_rail()
            .into_iter()
            .map(|item| {
                (
                    item.id,
                    item.name,
                    item.attention,
                    item.unread,
                    item.connection,
                    item.active,
                )
            })
            .collect();
        if rail != self.last_rail {
            self.last_rail = rail;
            out.changes.push(Change::RailChanged);
        }
    }
}

// ----- Inputs -----

impl ClientCore {
    pub(super) fn workspace_input(&mut self, input: Input, out: &mut Output) {
        match input {
            Input::SwitchWorkspace(workspace) => self.switch_workspace(workspace, out),
            Input::CreateWorkspace { name } => self.create_workspace(&name, out),
            Input::AcceptInvitation { code } => self.accept_invitation(&code, out),
            Input::UpdateProfile { display_name } => self.update_profile(display_name, out),
            Input::SetAvatar { source } => self.set_avatar(source, out),
            Input::RemoveAvatar => self.remove_avatar(out),
            Input::LoadAvatar(user) => self.load_avatar(user, out),
            Input::AvatarLoaded {
                user,
                version,
                result,
            } => self.avatar_loaded(user, version, result, out),
            Input::SetAppearance(appearance) if self.appearance != appearance => {
                self.appearance = appearance;
                out.effects.push(Effect::PersistAppearance(appearance));
                out.changes.push(Change::AppearanceChanged);
            }
            _ => {}
        }
    }

    fn session_ready(&self) -> bool {
        matches!(self.phase, SessionPhase::Ready)
    }

    // ----- Switching -----

    /// Closes the views of the loaded workspace that belong to it alone.
    fn close_scoped_views(&mut self, out: &mut Output) {
        self.close_search(out);
        self.close_inbox(out);
        self.close_saved(out);
        self.close_people(out);
        self.close_profile(out);
        self.close_browse(out);
        self.close_admin_views(out);
        self.close_pane(out);
    }

    /// Makes `workspace` the active one: the screen shows its conversations, its people, its
    /// settings, and the conversation that was open in it last.
    pub(super) fn switch_workspace(&mut self, workspace: WorkspaceId, out: &mut Output) {
        if self.active == Some(workspace) || !self.knows_workspace(workspace) {
            return;
        }
        self.close_scoped_views(out);
        // What was being jumped to belongs to the workspace left.
        self.set_jump(None, out);
        self.activate(workspace, out);
    }

    /// Loads `workspace`'s scope as the active one and selects what it was showing.
    fn activate(&mut self, workspace: WorkspaceId, out: &mut Output) {
        self.enter_scope(Some(workspace));
        self.active = Some(workspace);
        out.changes.push(Change::ActiveWorkspaceChanged);
        out.changes.push(Change::DirectoryChanged);
        out.changes.push(Change::RailChanged);
        out.effects
            .push(Effect::PersistActiveWorkspace(Some(workspace)));
        // The conversation open in the workspace left stays where it is (its timeline is kept);
        // what is selected now is the new workspace's own.
        let next = self.default_selection();
        let previous = self.selected;
        if previous.is_some_and(|id| self.conversations.iter().any(|c| c.id == id)) {
            // Selected before and belongs here: nothing to change.
        } else {
            self.selected_clear_for_switch();
            self.change_selection(next, out);
        }
        if let Some(selected) = self.selected {
            self.ensure_history(selected, out);
        }
        self.cache_changed(true);
    }

    /// Forgets the selection of the workspace left without the work of `change_selection` (the
    /// timeline and read state of its conversation are kept, since they are keyed by id).
    fn selected_clear_for_switch(&mut self) {
        if let Some(previous) = self.selected.take() {
            self.marker_anchor.remove(&previous);
            self.at_bottom.remove(&previous);
            self.mark_failed.remove(&previous);
        }
    }

    // ----- Creating and joining -----

    fn create_workspace(&mut self, name: &str, out: &mut Output) {
        let kind = ActionKind::CreateWorkspace;
        if !self.session_ready() || self.action(kind) == &ActionState::InFlight {
            return;
        }
        let name = match WorkspaceName::parse(name) {
            Ok(name) => name,
            Err(error) => return self.fail_action(kind, error.to_string(), out),
        };
        let workspace = WorkspaceId::generate();
        self.creating = Some(CreateJob {
            workspace,
            name: name.clone(),
            attempts: 1,
        });
        self.set_action(kind, ActionState::InFlight, out);
        out.effects.push(Effect::CreateWorkspace {
            workspace,
            request: CreateWorkspaceRequest { name },
        });
    }

    /// An unknown outcome of creating a workspace is tried again with the same id (the request is
    /// idempotent) up to three times. Returns whether it was.
    pub(super) fn retry_create_workspace(&mut self, out: &mut Output) -> bool {
        let Some(job) = self.creating.as_mut() else {
            return false;
        };
        if job.attempts >= MAX_CREATE_ATTEMPTS {
            return false;
        }
        job.attempts += 1;
        out.effects.push(Effect::CreateWorkspace {
            workspace: job.workspace,
            request: CreateWorkspaceRequest {
                name: job.name.clone(),
            },
        });
        true
    }

    fn accept_invitation(&mut self, code: &str, out: &mut Output) {
        let kind = ActionKind::AcceptInvitation;
        if !self.session_ready() || self.action(kind) == &ActionState::InFlight {
            return;
        }
        let code = code.trim();
        if code.is_empty() {
            return self.fail_action(kind, "Enter the invitation code.", out);
        }
        if !invite_code_is_wellformed(code) {
            return self.fail_action(kind, MALFORMED_INVITE_CODE, out);
        }
        self.set_action(kind, ActionState::InFlight, out);
        out.effects.push(Effect::AcceptInvitation {
            request: AcceptInvitationRequest {
                code: InviteCode::new(code),
            },
        });
    }

    /// The server returned a workspace the person created or joined: it is listed, subscribed to
    /// after its `head`, and made active. Joining one the person already belongs to just shows it.
    pub(super) fn workspace_joined(&mut self, joined: WorkspaceBootstrap, out: &mut Output) {
        self.creating = None;
        let id = joined.workspace.id;
        if self.knows_workspace(id) {
            self.switch_workspace(id, out);
            return;
        }
        let me = self.me.clone();
        self.workspaces.push(WorkspaceInfo {
            workspace: joined.workspace.clone(),
            role: joined.role,
        });
        let ids = self.members.entry(id).or_default();
        ids.extend(joined.members.iter().map(|member| member.id));
        self.in_scope(id, |core| core.fill_scope(&joined, me.as_ref(), out));
        self.adopt_joined_states(&joined);
        self.applied.insert(id, joined.head);
        self.subscribed.insert(id);
        self.sockets.insert(id, ConnectionState::Connecting);
        out.effects.push(Effect::Subscribe {
            workspace: id,
            after: joined.head,
        });
        self.recompute_connection(out);
        // The list just taken is the newest the core has of these people: wherever else it knows
        // them, it knows them as this says.
        for member in &joined.members {
            self.apply_profile(
                member.id,
                member.display_name.clone(),
                member.avatar_version,
                out,
            );
        }
        self.reconcile_avatars(out);
        self.close_scoped_views(out);
        self.activate(id, out);
    }

    /// Fills the loaded scope from a workspace's bootstrap.
    pub(super) fn fill_scope(
        &mut self,
        bootstrap: &WorkspaceBootstrap,
        me: Option<&User>,
        out: &mut Output,
    ) {
        self.users = bootstrap
            .members
            .iter()
            .map(|member| (member.id, member.clone()))
            .collect();
        if let Some(me) = me {
            self.users.entry(me.id).or_insert_with(|| me.clone());
        }
        self.conversations = bootstrap.conversations.clone();
        self.adopt_workspace_groups(bootstrap);
        self.adopt_workspace_emoji(bootstrap);
        self.adopt_workspace_settings(
            bootstrap,
            me.and_then(|me| {
                bootstrap
                    .members
                    .iter()
                    .find(|member| member.id == me.id)
                    .map(|member| member.status.clone())
            }),
            out,
        );
        out.changes.push(Change::DirectoryChanged);
    }

    // ----- Profile -----

    /// Takes the read states and thread states of a workspace that was just created or joined.
    fn adopt_joined_states(&mut self, joined: &WorkspaceBootstrap) {
        let Some(me) = self.me.clone() else {
            return;
        };
        let response = lazy_protocol::BootstrapResponse {
            protocol_version: lazy_protocol::PROTOCOL_VERSION,
            user: me,
            workspaces: vec![joined.clone()],
        };
        self.adopt_bootstrap_states(&response, false);
        self.adopt_bootstrap_thread_states(&response, false);
    }

    fn update_profile(&mut self, display_name: Option<String>, out: &mut Output) {
        let kind = ActionKind::UpdateProfile;
        if !self.session_ready() || self.action(kind) == &ActionState::InFlight {
            return;
        }
        let display_name = match display_name {
            None => return,
            Some(text) => match DisplayName::parse(&text) {
                Ok(name) => name,
                Err(error) => return self.fail_action(kind, error.to_string(), out),
            },
        };
        if self
            .me
            .as_ref()
            .is_some_and(|me| me.display_name == display_name)
        {
            return;
        }
        self.set_action(kind, ActionState::InFlight, out);
        out.effects.push(Effect::UpdateProfile {
            request: UpdateProfileRequest {
                display_name: Some(display_name),
            },
        });
    }

    fn set_avatar(&mut self, source: FileSource, out: &mut Output) {
        let kind = ActionKind::SetAvatar;
        if !self.session_ready() || self.action(kind) == &ActionState::InFlight {
            return;
        }
        if let FileSource::Bytes(bytes) = &source
            && bytes.len() > MAX_AVATAR_UPLOAD_BYTES
        {
            let text = format!(
                "That picture is too large. A picture can be at most {} MiB.",
                MAX_AVATAR_UPLOAD_BYTES / (1024 * 1024)
            );
            return self.fail_action(kind, text, out);
        }
        self.set_action(kind, ActionState::InFlight, out);
        out.effects.push(Effect::SetAvatar { source });
    }

    fn remove_avatar(&mut self, out: &mut Output) {
        let kind = ActionKind::SetAvatar;
        if !self.session_ready() || self.action(kind) == &ActionState::InFlight {
            return;
        }
        if self.me.as_ref().is_none_or(|me| me.avatar_version == 0) {
            return;
        }
        self.set_action(kind, ActionState::InFlight, out);
        out.effects.push(Effect::RemoveAvatar);
    }

    /// The server's answer to a change of the person's own profile: they are now as it says
    /// (everywhere they appear).
    pub(super) fn profile_changed_by_me(&mut self, user: User, out: &mut Output) {
        self.apply_profile(user.id, user.display_name, user.avatar_version, out);
    }

    /// Someone's display name or avatar changed. It is the same person in every workspace, so
    /// every workspace's copy follows (and the person's own record).
    pub(super) fn apply_profile(
        &mut self,
        user: UserId,
        display_name: DisplayName,
        avatar_version: u32,
        out: &mut Output,
    ) {
        let mut changed = false;
        let mut versions = Vec::new();
        let mut apply = |held: &mut User| {
            if held.display_name != display_name || held.avatar_version != avatar_version {
                versions.push(held.avatar_version);
                held.display_name = display_name.clone();
                held.avatar_version = avatar_version;
                changed = true;
            }
        };
        if let Some(held) = self.users.get_mut(&user) {
            apply(held);
        }
        for scope in self.parked.values_mut() {
            if let Some(held) = scope.users.get_mut(&user) {
                apply(held);
            }
        }
        if let Some(held) = self.me.as_mut().filter(|me| me.id == user) {
            apply(held);
        }
        if changed {
            out.changes.push(Change::DirectoryChanged);
            self.cache_changed(true);
            self.avatar_version_known(user, avatar_version, out);
        }
    }

    // ----- Avatars -----

    /// The version of a person's avatar as far as the core knows: the person's own record, the
    /// people of any workspace, the directory page(s) held, and the loaded profile. 0 when there
    /// is none (or the person is not known).
    pub fn avatar_version(&self, user: UserId) -> u32 {
        self.known_avatar_version(user)
    }

    fn known_avatar_version(&self, user: UserId) -> u32 {
        self.me
            .as_ref()
            .filter(|me| me.id == user)
            .or_else(|| self.find_user(user))
            // The directory pages the whole workspace and a profile is fetched on its own: people
            // there need not be among the members the bootstrap lists.
            .or_else(|| {
                self.people()
                    .people
                    .iter()
                    .map(|person| &person.user)
                    .find(|held| held.id == user)
            })
            .or_else(|| {
                self.profile()
                    .and_then(|profile| profile.person.as_ref())
                    .map(|person| &person.user)
                    .filter(|held| held.id == user)
            })
            .map_or(0, |held| held.avatar_version)
    }

    /// A person's avatar is at `version` now: a picture held of another version goes, and one that
    /// was wanted is fetched again.
    fn avatar_version_known(&mut self, user: UserId, version: u32, out: &mut Output) {
        let Some((held, _)) = self.avatars.images.get(&user) else {
            return;
        };
        if *held == version {
            return;
        }
        self.drop_avatar(user);
        out.effects
            .push(Effect::ForgetAvatars { users: vec![user] });
        out.changes.push(Change::AvatarChanged { user });
        if version > 0 {
            self.load_avatar(user, out);
        }
    }

    /// After people were taken from a bootstrap: pictures held of a version that is no longer
    /// theirs go, and the ones that were wanted are fetched again.
    pub(super) fn reconcile_avatars(&mut self, out: &mut Output) {
        let held: Vec<UserId> = self.avatars.images.keys().copied().collect();
        for user in held {
            let version = self.known_avatar_version(user);
            self.avatar_version_known(user, version, out);
        }
    }

    fn drop_avatar(&mut self, user: UserId) {
        if let Some((_, ImageState::Ready(data))) = self.avatars.images.remove(&user) {
            self.avatars.bytes = self.avatars.bytes.saturating_sub(data.len());
        }
        self.avatars.order.retain(|held| *held != user);
        self.avatars.queue.retain(|held| *held != user);
    }

    fn load_avatar(&mut self, user: UserId, out: &mut Output) {
        let version = self.known_avatar_version(user);
        if version == 0 {
            if self.avatars.images.contains_key(&user) {
                self.drop_avatar(user);
                out.changes.push(Change::AvatarChanged { user });
            }
            return;
        }
        match self.avatars.images.get(&user) {
            Some((held, ImageState::Ready(_))) if *held == version => {
                // Used now: last to be dropped.
                self.avatars.order.retain(|other| *other != user);
                self.avatars.order.push_back(user);
                return;
            }
            Some((held, ImageState::Loading | ImageState::Failed)) if *held == version => {
                if matches!(
                    self.avatars.images.get(&user),
                    Some((_, ImageState::Failed))
                ) {
                    // Asking again after a failure tries again.
                } else {
                    return;
                }
            }
            _ => {}
        }
        self.drop_avatar(user);
        self.avatars
            .images
            .insert(user, (version, ImageState::Loading));
        self.avatars.queue.push_back(user);
        while self.avatars.queue.len() > MAX_QUEUED_AVATAR_FETCHES {
            if let Some(dropped) = self.avatars.queue.pop_front()
                && matches!(
                    self.avatars.images.get(&dropped),
                    Some((_, ImageState::Loading))
                )
            {
                self.avatars.images.remove(&dropped);
                out.changes.push(Change::AvatarChanged { user: dropped });
            }
        }
        out.changes.push(Change::AvatarChanged { user });
        self.pump_avatars(out);
    }

    fn pump_avatars(&mut self, out: &mut Output) {
        while self.avatars.in_flight.len() < crate::attachments::MAX_CONCURRENT_IMAGE_FETCHES {
            let Some(user) = self.avatars.queue.pop_front() else {
                break;
            };
            let Some((version, ImageState::Loading)) = self.avatars.images.get(&user) else {
                continue;
            };
            let version = *version;
            if self.avatars.in_flight.insert((user, version)) {
                out.effects.push(Effect::FetchAvatar { user, version });
            }
        }
    }

    fn avatar_loaded(
        &mut self,
        user: UserId,
        version: u32,
        result: Result<ImageData, TransferError>,
        out: &mut Output,
    ) {
        self.avatars.in_flight.remove(&(user, version));
        // Only the version wanted now is kept; an older one that was on its way is dropped.
        if !matches!(
            self.avatars.images.get(&user),
            Some((held, ImageState::Loading)) if *held == version
        ) {
            self.pump_avatars(out);
            return;
        }
        match result {
            Ok(data) => {
                self.avatars.bytes += data.len();
                self.avatars
                    .images
                    .insert(user, (version, ImageState::Ready(data)));
                self.avatars.order.push_back(user);
                self.evict_avatars(user);
            }
            Err(TransferError::Request(error)) if error.is_unauthenticated() => {
                self.avatars.images.remove(&user);
                self.session_expired(out);
            }
            Err(TransferError::Cancelled) => {
                self.avatars.images.remove(&user);
            }
            Err(_) => {
                self.avatars
                    .images
                    .insert(user, (version, ImageState::Failed));
            }
        }
        out.changes.push(Change::AvatarChanged { user });
        self.pump_avatars(out);
    }

    fn evict_avatars(&mut self, keep: UserId) {
        while (self.avatars.order.len() > MAX_HELD_AVATARS
            || self.avatars.bytes > MAX_HELD_AVATAR_BYTES)
            && self.avatars.order.len() > 1
        {
            let Some(old) = self.avatars.order.pop_front() else {
                break;
            };
            if old == keep {
                self.avatars.order.push_back(old);
                continue;
            }
            if let Some((_, ImageState::Ready(data))) = self.avatars.images.remove(&old) {
                self.avatars.bytes = self.avatars.bytes.saturating_sub(data.len());
            }
        }
    }

    /// The connection is back: pictures that could not be fetched are asked for again the next
    /// time they are wanted.
    pub(super) fn retry_failed_avatars(&mut self, out: &mut Output) {
        let failed: Vec<UserId> = self
            .avatars
            .images
            .iter()
            .filter(|(_, (_, state))| *state == ImageState::Failed)
            .map(|(user, _)| *user)
            .collect();
        for user in failed {
            self.avatars.images.remove(&user);
            out.changes.push(Change::AvatarChanged { user });
        }
    }

    // ----- Answers -----

    /// Applies the result of one of this module's actions. Returns whether it was one.
    pub(super) fn workspace_action_result(
        &mut self,
        kind: ActionKind,
        outcome: ActionOutcome,
        out: &mut Output,
    ) -> bool {
        match (kind, outcome) {
            (
                ActionKind::CreateWorkspace | ActionKind::AcceptInvitation,
                ActionOutcome::Joined(joined),
            ) => self.workspace_joined(*joined, out),
            (ActionKind::UpdateProfile | ActionKind::SetAvatar, ActionOutcome::Profile(user)) => {
                self.profile_changed_by_me(user, out)
            }
            _ => return false,
        }
        true
    }
}

/// Words for a person about why creating or joining a workspace failed.
pub(super) fn workspace_error(kind: ActionKind, error: &RequestError) -> String {
    use lazy_protocol::ErrorCode;
    let RequestError::Rejected(api) = error else {
        return super::conversations::readable_error(error);
    };
    let said = (!api.message.is_empty()).then(|| api.message.clone());
    match (kind, api.code) {
        (ActionKind::AcceptInvitation, ErrorCode::InvitationInvalid) => {
            "That invite code isn't valid. It may have expired, been used up, or been revoked. \
             Ask for a new one."
                .to_owned()
        }
        (ActionKind::AcceptInvitation, ErrorCode::Forbidden) => said.unwrap_or_else(|| {
            "Your membership of that workspace is deactivated, so you can't rejoin it.".to_owned()
        }),
        (ActionKind::CreateWorkspace, ErrorCode::LimitReached) => format!(
            "You have created as many workspaces as you can (at most {}).",
            lazy_domain::workspace::MAX_WORKSPACES_CREATED
        ),
        (ActionKind::CreateWorkspace, ErrorCode::Forbidden) => {
            said.unwrap_or_else(|| "This server doesn't allow creating workspaces.".to_owned())
        }
        (ActionKind::CreateWorkspace, ErrorCode::Conflict) => {
            said.unwrap_or_else(|| "That workspace already exists. Try again.".to_owned())
        }
        (ActionKind::SetAvatar, ErrorCode::UnsupportedMedia) => {
            "That picture can't be used. Choose a PNG, JPEG, WebP, or GIF picture.".to_owned()
        }
        (ActionKind::SetAvatar, ErrorCode::PayloadTooLarge) => format!(
            "That picture is too large. A picture can be at most {} MiB.",
            MAX_AVATAR_UPLOAD_BYTES / (1024 * 1024)
        ),
        (_, ErrorCode::InvalidRequest | ErrorCode::NameTaken) => {
            said.unwrap_or_else(|| super::conversations::readable_error(error))
        }
        _ => super::conversations::readable_error(error),
    }
}

/// The attention that unmuted unread messages ask of the person.
impl ClientCore {
    pub(super) fn attention_of(&self, conversations: &[Conversation]) -> u32 {
        let mut total = 0u32;
        let mut known: HashSet<ConversationId> = HashSet::new();
        for conversation in conversations {
            known.insert(conversation.id);
            if self.is_muted(conversation.id) {
                continue;
            }
            let Some(state) = self.user_states.get(&conversation.id) else {
                continue;
            };
            let counted = if conversation.kind == ConversationKind::Channel {
                state.mention_count
            } else {
                state.unread_count
            };
            total = total.saturating_add(counted);
        }
        for state in self.thread_states.values() {
            if state.following
                && known.contains(&state.conversation_id)
                && !self.is_muted(state.conversation_id)
            {
                total = total.saturating_add(state.unread_count);
            }
        }
        total
    }

    /// Whether there are unread messages in unmuted conversations (the rail shows a dot).
    pub(super) fn unread_of(&self, conversations: &[Conversation]) -> bool {
        conversations.iter().any(|conversation| {
            !conversation.archived
                && !self.is_muted(conversation.id)
                && self
                    .user_states
                    .get(&conversation.id)
                    .is_some_and(|state| state.unread_count > 0)
        })
    }
}

/// Up to two letters standing for a workspace's name: the first letters of its first two words,
/// or its first two letters when it is one word; uppercase.
pub(super) fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let letters: Vec<char> = match words.as_slice() {
        [] => Vec::new(),
        [one] => one.chars().take(2).collect(),
        [a, b, ..] => [a, b]
            .iter()
            .filter_map(|word| word.chars().next())
            .collect(),
    };
    letters.into_iter().flat_map(char::to_uppercase).collect()
}
