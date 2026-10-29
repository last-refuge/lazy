//! Workspace administration (slice 3F, D22): the workspace's name and settings, people's roles and
//! deactivation, invitations, and what the person is offered because of their role.
//!
//! The server decides who may do what; the core uses `lazy_domain::workspace` only to decide what
//! to offer and to refuse what cannot work without asking. Every change here is an idempotent
//! request that is not journaled: the answer updates what is shown, a failure keeps the old value
//! and says why. Creating an invitation is the exception to repeating: it is never repeated, and
//! an unknown outcome is reconciled by listing.

use lazy_domain::workspace::{
    MAX_ACTIVE_INVITATIONS, MAX_INVITE_TTL_SECS, MAX_INVITE_USES, MIN_INVITE_TTL_SECS,
    WorkspaceSettings, check_member_action, check_role_change,
};
use lazy_domain::{InvitationId, UserId, WorkspaceName, WorkspaceRole};
use lazy_protocol::{
    CreateInvitationRequest, CreatedInvitation, ErrorCode, Invitation, InvitationList,
    SetDeactivatedRequest, SetRoleRequest, UpdateWorkspaceRequest, Workspace,
};

use crate::admin::{CreatedInvite, InvitationsView};
use crate::change::{Change, Output};
use crate::effect::Effect;
use crate::input::Input;
use crate::model::{ActionKind, ActionState, RequestError, SessionPhase, WorkspaceInfo};
use crate::notify::LoadStatus;

use super::ClientCore;
use super::conversations::readable_error;
use super::discovery::Pager;

/// What the core keeps about administration.
#[derive(Debug, Default)]
pub(super) struct Administration {
    invitations: InvitationsView,
    pager: Pager,
    /// The workspace's name as last known, for "You no longer have access to …".
    known_name: Option<String>,
    /// The name of a workspace the server stopped listing.
    lost: Option<String>,
    /// The sign-in screen's last request was a registration (so its refusals are worded for one).
    registering: bool,
}

/// Why a registration was refused locally because of its invite code. It never repeats the code.
pub(super) const MALFORMED_INVITE_CODE: &str =
    "That isn't a valid invite code. A code is 20 to 64 letters, digits, '-' or '_'.";

impl Administration {
    /// Exchanges the invitations of one workspace and leaves the counter, the name of a workspace
    /// that was lost, and the registration flag.
    pub(super) fn swap_scoped(&mut self, other: &mut Self) {
        std::mem::swap(&mut self.invitations, &mut other.invitations);
        self.pager.swap_flight(&mut other.pager);
    }

    pub(super) fn set_registering(&mut self, registering: bool) {
        self.registering = registering;
    }

    /// Words for a refused sign-in or registration. A registration the server refused for its
    /// invitation, or for the workspace being invite-only, says so.
    pub(super) fn auth_failure(&self, error: &RequestError) -> String {
        match error {
            RequestError::Rejected(api) if self.registering => match api.code {
                ErrorCode::InvitationInvalid => "That invite code isn't valid. It may have \
                     expired, been used up, or been revoked. Ask for a new one."
                    .to_owned(),
                ErrorCode::Forbidden => "This workspace is invite-only. Ask an admin for an \
                     invite code and enter it to join."
                    .to_owned(),
                _ => error.message(),
            },
            _ => error.message(),
        }
    }
}

impl ClientCore {
    // ----- Reading -----

    /// The settings of the person's workspace (the defaults before one is known).
    pub fn workspace_settings(&self) -> WorkspaceSettings {
        self.primary_workspace()
            .map(|info| info.workspace.settings)
            .unwrap_or_default()
    }

    /// The person's role in their workspace, as it stands now.
    pub fn my_role(&self) -> Option<WorkspaceRole> {
        self.primary_workspace().map(|info| info.role)
    }

    /// Whether the person may change the workspace, invite people, and manage members.
    pub fn can_manage_workspace(&self) -> bool {
        self.my_role()
            .is_some_and(WorkspaceRole::can_manage_workspace)
    }

    /// Whether the person may create a channel: their role, and the workspace's policy.
    pub fn can_create_channel(&self) -> bool {
        self.my_role()
            .is_some_and(|role| self.workspace_settings().channel_creation.allows(role))
    }

    /// The invitations, once opened with `Input::OpenInvitations`.
    pub fn invitations(&self) -> &InvitationsView {
        &self.admin.invitations
    }

    /// The name of the workspace the person no longer has access to: the session is ready but the
    /// server stopped listing the workspace (the person was deactivated). `None` otherwise,
    /// including for an account that never had a workspace.
    pub fn access_lost(&self) -> Option<&str> {
        self.admin.lost.as_deref()
    }

    /// Whether the core knows `user` to be deactivated in the workspace.
    pub(super) fn is_deactivated(&self, user: UserId) -> bool {
        self.users.get(&user).is_some_and(|user| user.deactivated)
    }

    /// A person's role, if a directory or profile that is loaded says what it is.
    fn known_role(&self, user: UserId) -> Option<WorkspaceRole> {
        self.people()
            .people
            .iter()
            .find(|person| person.user.id == user)
            .map(|person| person.role)
            .or_else(|| {
                self.profile()
                    .and_then(|profile| profile.person.as_ref())
                    .filter(|person| person.user.id == user)
                    .map(|person| person.role)
            })
    }

    // ----- Inputs -----

    pub(super) fn admin_input(&mut self, input: Input, out: &mut Output) {
        match input {
            Input::UpdateWorkspace { name, settings } => self.update_workspace(name, settings, out),
            Input::SetMemberRole { user_id, role } => self.set_member_role(user_id, role, out),
            Input::SetMemberDeactivated {
                user_id,
                deactivated,
            } => self.set_member_deactivated(user_id, deactivated, out),
            Input::OpenInvitations => self.open_invitations(out),
            Input::CloseInvitations => self.close_invitations(out),
            Input::CreateInvitation {
                expires_in_secs,
                max_uses,
            } => self.create_invitation(expires_in_secs, max_uses, out),
            Input::RevokeInvitation(id) => self.revoke_invitation(id, out),
            Input::DismissCreatedInvitation => {
                if self.admin.invitations.created.take().is_some() {
                    out.changes.push(Change::InvitationsChanged);
                }
            }
            Input::InvitationsLoaded { generation, result } => {
                self.invitations_loaded(generation, result, out)
            }
            _ => {}
        }
    }

    fn ready_to_administer(&self) -> bool {
        matches!(self.phase, SessionPhase::Ready)
    }

    fn update_workspace(
        &mut self,
        name: Option<String>,
        settings: Option<WorkspaceSettings>,
        out: &mut Output,
    ) {
        let kind = ActionKind::UpdateWorkspace;
        if self.action(kind) == &ActionState::InFlight || !self.ready_to_administer() {
            return;
        }
        if !self.can_manage_workspace() {
            let message = "Only admins and owners can change the workspace.";
            return self.fail_action(kind, message, out);
        }
        let name = match name {
            None => None,
            Some(text) => match WorkspaceName::parse(&text) {
                Ok(name) => Some(name),
                Err(error) => return self.fail_action(kind, error.to_string(), out),
            },
        };
        if name.is_none() && settings.is_none() {
            return;
        }
        let Some(workspace) = self.primary_workspace_id() else {
            return self.fail_action(kind, "Your workspace hasn't loaded yet.", out);
        };
        if self.begin_action(kind, out) {
            out.effects.push(Effect::UpdateWorkspace {
                workspace,
                request: UpdateWorkspaceRequest { name, settings },
            });
        }
    }

    fn set_member_role(&mut self, user: UserId, role: WorkspaceRole, out: &mut Output) {
        let kind = ActionKind::SetMemberRole(user);
        if self.action(kind) == &ActionState::InFlight || !self.ready_to_administer() {
            return;
        }
        let Some(actor) = self.my_role() else {
            return self.fail_action(kind, "Your workspace hasn't loaded yet.", out);
        };
        if !actor.can_manage_workspace() {
            let message = "Only admins and owners can change roles.";
            return self.fail_action(kind, message, out);
        }
        // Who may set whose role is decided here only as far as the role is known; whether another
        // owner remains is the server's to say (it counts under the workspace lock).
        if let Some(target) = self.known_role(user)
            && check_role_change(actor, target, role, 1).is_err()
        {
            return self.fail_action(kind, "You can't change this person's role.", out);
        }
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        if self.begin_action(kind, out) {
            out.effects.push(Effect::SetMemberRole {
                workspace,
                user,
                request: SetRoleRequest { role },
            });
        }
    }

    fn set_member_deactivated(&mut self, user: UserId, deactivated: bool, out: &mut Output) {
        let kind = ActionKind::SetMemberDeactivated(user);
        if self.action(kind) == &ActionState::InFlight || !self.ready_to_administer() {
            return;
        }
        let Some(actor) = self.my_role() else {
            return self.fail_action(kind, "Your workspace hasn't loaded yet.", out);
        };
        let is_self = self.my_id() == Some(user);
        if is_self {
            return self.fail_action(kind, "You can't deactivate yourself.", out);
        }
        if !actor.can_manage_workspace() {
            let message = "Only admins and owners can deactivate people.";
            return self.fail_action(kind, message, out);
        }
        if let Some(target) = self.known_role(user)
            && check_member_action(actor, target, false).is_err()
        {
            let message = "You can't deactivate this person.";
            return self.fail_action(kind, message, out);
        }
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        if self.begin_action(kind, out) {
            out.effects.push(Effect::SetMemberDeactivated {
                workspace,
                user,
                request: SetDeactivatedRequest { deactivated },
            });
        }
    }

    // ----- Invitations -----

    fn open_invitations(&mut self, out: &mut Output) {
        if !self.ready_to_administer() || !self.can_manage_workspace() {
            return;
        }
        let view = &mut self.admin.invitations;
        if view.open {
            // Opening a list that failed to load is asking again.
            if matches!(view.status, LoadStatus::Failed { .. }) && view.items.is_empty() {
                self.start_invitations_request(out);
            }
            return;
        }
        view.open = true;
        self.start_invitations_request(out);
    }

    fn close_invitations(&mut self, out: &mut Output) {
        if !self.admin.invitations.open && self.admin.invitations.created.is_none() {
            return;
        }
        if self.admin.pager.cancel() {
            out.effects.push(Effect::CancelInvitations);
        }
        // The code goes with the view.
        self.admin.invitations = InvitationsView::default();
        out.changes.push(Change::InvitationsChanged);
    }

    fn start_invitations_request(&mut self, out: &mut Output) {
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        let generation = self.admin.pager.begin(true);
        let view = &mut self.admin.invitations;
        view.status = LoadStatus::Loading;
        view.items.clear();
        out.effects.push(Effect::LoadInvitations {
            workspace,
            generation,
        });
        out.changes.push(Change::InvitationsChanged);
    }

    /// Asks for the list again without disturbing what is shown.
    pub(super) fn reload_invitations(&mut self, out: &mut Output) {
        if !self.admin.invitations.open {
            return;
        }
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        let generation = self.admin.pager.begin(true);
        out.effects.push(Effect::LoadInvitations {
            workspace,
            generation,
        });
    }

    fn invitations_loaded(
        &mut self,
        generation: u64,
        result: Result<InvitationList, RequestError>,
        out: &mut Output,
    ) {
        if !self.admin.invitations.open || self.admin.pager.accept(generation).is_none() {
            return;
        }
        match result {
            Ok(list) => {
                let view = &mut self.admin.invitations;
                view.items = list.invitations;
                view.status = LoadStatus::Ready;
            }
            Err(error) if error.is_unauthenticated() => return self.session_expired(out),
            Err(error) => {
                let (reason, can_retry) = match &error {
                    RequestError::Rejected(api)
                        if matches!(api.code, ErrorCode::Forbidden | ErrorCode::NotFound) =>
                    {
                        (
                            "Only admins and owners can see invitations.".to_owned(),
                            false,
                        )
                    }
                    RequestError::Rejected(api) if api.code == ErrorCode::RateLimited => (
                        "Too many requests. Wait a moment and try again.".to_owned(),
                        true,
                    ),
                    RequestError::Rejected(_) => ("Something went wrong.".to_owned(), true),
                    RequestError::Unknown(_) => ("Couldn't reach the server.".to_owned(), true),
                };
                self.admin.invitations.status = LoadStatus::Failed { reason, can_retry };
            }
        }
        out.changes.push(Change::InvitationsChanged);
    }

    fn create_invitation(
        &mut self,
        expires_in_secs: Option<u64>,
        max_uses: Option<u32>,
        out: &mut Output,
    ) {
        let kind = ActionKind::CreateInvitation;
        if self.action(kind) == &ActionState::InFlight
            || !self.ready_to_administer()
            || !self.admin.invitations.open
        {
            return;
        }
        if !self.can_manage_workspace() {
            let message = "Only admins and owners can invite people.";
            return self.fail_action(kind, message, out);
        }
        if let Some(secs) = expires_in_secs
            && !(MIN_INVITE_TTL_SECS..=MAX_INVITE_TTL_SECS).contains(&secs)
        {
            let message = "An invitation can last from 1 hour to 30 days.";
            return self.fail_action(kind, message, out);
        }
        if let Some(uses) = max_uses
            && !(1..=MAX_INVITE_USES).contains(&uses)
        {
            let message = format!("An invitation can be used from 1 to {MAX_INVITE_USES} times.");
            return self.fail_action(kind, message, out);
        }
        let Some(workspace) = self.primary_workspace_id() else {
            return self.fail_action(kind, "Your workspace hasn't loaded yet.", out);
        };
        // A code shown before is gone once another is made.
        self.admin.invitations.created = None;
        if self.begin_action(kind, out) {
            out.effects.push(Effect::CreateInvitation {
                workspace,
                request: CreateInvitationRequest {
                    expires_in_secs,
                    max_uses,
                },
            });
        }
    }

    fn revoke_invitation(&mut self, id: InvitationId, out: &mut Output) {
        let kind = ActionKind::RevokeInvitation(id);
        if self.action(kind) == &ActionState::InFlight || !self.ready_to_administer() {
            return;
        }
        if !self.can_manage_workspace() {
            let message = "Only admins and owners can revoke invitations.";
            return self.fail_action(kind, message, out);
        }
        if self.begin_action(kind, out) {
            out.effects
                .push(Effect::RevokeInvitation { invitation: id });
        }
    }

    /// An unknown outcome of creating an invitation: not repeated, since it may have been made.
    /// The list shows what the server has.
    pub(super) fn invitation_create_lost(&mut self, out: &mut Output) {
        self.reload_invitations(out);
        let notice = "Couldn't tell whether the invitation was made. The list shows what the \
                      server has; if the new one is missing, make it again.";
        self.set_notice(Some(notice.to_owned()), out);
    }

    // ----- What an action's answer changes -----

    pub(super) fn admin_action_result(
        &mut self,
        kind: ActionKind,
        outcome: crate::model::ActionOutcome,
        out: &mut Output,
    ) {
        use crate::model::ActionOutcome;
        match (kind, outcome) {
            (ActionKind::UpdateWorkspace, ActionOutcome::Workspace(workspace)) => {
                self.apply_workspace(workspace, out)
            }
            (ActionKind::SetMemberRole(_), ActionOutcome::Role { user, role }) => {
                self.apply_role(user, role, out)
            }
            (
                ActionKind::SetMemberDeactivated(_),
                ActionOutcome::Deactivated { user, deactivated },
            ) => self.apply_deactivated(user, deactivated, out),
            (ActionKind::CreateInvitation, ActionOutcome::InvitationCreated(created)) => {
                self.invitation_created(created, out)
            }
            (ActionKind::RevokeInvitation(_), ActionOutcome::Invitation(invitation)) => {
                self.invitation_revoked(invitation, out)
            }
            _ => {}
        }
    }

    fn invitation_created(&mut self, created: CreatedInvitation, out: &mut Output) {
        let view = &mut self.admin.invitations;
        if !view.open {
            // The view was closed meanwhile: the code goes with it.
            return;
        }
        view.items.insert(0, created.invitation.clone());
        view.created = Some(CreatedInvite {
            invitation: created.invitation,
            code: created.code,
        });
        out.changes.push(Change::InvitationsChanged);
    }

    fn invitation_revoked(&mut self, invitation: Invitation, out: &mut Output) {
        let view = &mut self.admin.invitations;
        let mut changed = false;
        for held in &mut view.items {
            if held.id == invitation.id && *held != invitation {
                *held = invitation.clone();
                changed = true;
            }
        }
        if let Some(created) = view.created.as_mut()
            && created.invitation.id == invitation.id
        {
            created.invitation = invitation;
            changed = true;
        }
        if changed {
            out.changes.push(Change::InvitationsChanged);
        }
    }

    // ----- Events and answers that change the workspace -----

    /// The workspace's name or settings changed (an event, or the answer to the person's own
    /// change).
    pub(super) fn apply_workspace(&mut self, workspace: Workspace, out: &mut Output) {
        let mut changed = false;
        for info in &mut self.workspaces {
            if info.workspace.id == workspace.id && info.workspace != workspace {
                info.workspace = workspace.clone();
                changed = true;
            }
        }
        if changed {
            self.admin.known_name = Some(workspace.name);
            out.changes.push(Change::WorkspaceChanged);
            self.cache_changed(true);
        }
    }

    /// Someone's role changed: the directory and the profile show it, and the person's own role is
    /// what the interface offers.
    pub(super) fn apply_role(&mut self, user: UserId, role: WorkspaceRole, out: &mut Output) {
        let scope = self.scope;
        if self.my_id() == Some(user)
            && let Some(info) = self
                .workspaces
                .iter_mut()
                .find(|info| Some(info.workspace.id) == scope)
            && info.role != role
        {
            info.role = role;
            out.changes.push(Change::WorkspaceChanged);
            self.cache_changed(true);
            self.role_lost_rights(out);
        }
        self.directory_role_changed(user, role, out);
    }

    /// The person's own role changed: what only managers may see is closed to anyone who no
    /// longer is one.
    fn role_lost_rights(&mut self, out: &mut Output) {
        self.people_demoted(out);
        if !self.can_manage_workspace() {
            self.close_invitations(out);
        }
    }

    /// Someone was deactivated or reactivated.
    pub(super) fn apply_deactivated(&mut self, user: UserId, deactivated: bool, out: &mut Output) {
        if let Some(known) = self.users.get_mut(&user)
            && known.deactivated != deactivated
        {
            known.deactivated = deactivated;
            out.changes.push(Change::DirectoryChanged);
            self.cache_changed(true);
        }
        self.directory_deactivation_changed(user, deactivated, out);
    }

    // ----- The bootstrap -----

    /// Remembers the workspace's name, notices what changed in it (the role included, which
    /// takes the administration away from someone who is no longer a manager), and notices when
    /// the server stopped listing it.
    pub(super) fn adopt_bootstrap_access(
        &mut self,
        had: Option<WorkspaceInfo>,
        had_all: &[WorkspaceInfo],
        out: &mut Output,
    ) {
        if let Some(info) = self.primary_workspace().cloned() {
            self.admin.known_name = Some(info.workspace.name.clone());
            let changed = had.as_ref() != Some(&info);
            let regained = self.admin.lost.take().is_some();
            if changed || regained {
                out.changes.push(Change::WorkspaceChanged);
            }
            // A workspace that is no longer listed leaves the rail, with a word about it.
            let gone: Vec<&WorkspaceInfo> = had_all
                .iter()
                .filter(|old| !self.knows_workspace(old.workspace.id))
                .collect();
            if let Some(old) = gone.first() {
                let notice = format!("You no longer have access to {}.", old.workspace.name);
                self.set_notice(Some(notice), out);
            }
            if !self.can_manage_workspace() {
                self.role_lost_rights(out);
            }
            return;
        }
        let name = had
            .map(|info| info.workspace.name)
            .or_else(|| self.admin.known_name.clone());
        if let Some(name) = name
            && self.admin.lost.as_deref() != Some(name.as_str())
        {
            self.admin.lost = Some(name);
            // What was cached belongs to a workspace the person cannot open any more.
            if self.cache_owner.take().is_some() {
                self.note_error("cache", "discarded_workspace_lost");
                out.effects.push(Effect::DiscardCache);
            }
            // There is nothing left to administer, no code to show, and nowhere to send the
            // changes that were waiting to be saved.
            self.close_invitations(out);
            self.clear_notifications(out);
            self.discovery_access_lost(out);
            out.changes.push(Change::WorkspaceChanged);
        }
    }

    /// The workspace held now, before a bootstrap replaces it.
    pub(super) fn held_workspace(&self) -> Option<WorkspaceInfo> {
        self.primary_workspace().cloned()
    }

    /// Forgets everything (sign-out, or another person signing in), the shown code included.
    pub(super) fn clear_admin(&mut self, out: &mut Output) {
        if self.admin.pager.cancel() {
            out.effects.push(Effect::CancelInvitations);
        }
        let last = self.admin.pager.last;
        self.admin = Administration::default();
        self.admin.pager.last = last;
        out.changes.push(Change::InvitationsChanged);
    }

    /// The session ended: the code is not kept.
    pub(super) fn close_admin_views(&mut self, out: &mut Output) {
        self.close_invitations(out);
    }
}

/// Words for a person about why a workspace administration request failed. The server's own
/// refusal is shown when it gave one, since it knows why (the last owner, an admin acting on an
/// owner).
pub(super) fn admin_error(kind: ActionKind, error: &RequestError) -> String {
    let RequestError::Rejected(api) = error else {
        return readable_error(error);
    };
    let said = (!api.message.is_empty()).then(|| api.message.clone());
    match (api.code, kind) {
        (ErrorCode::LimitReached, ActionKind::CreateInvitation) => format!(
            "A workspace can have at most {MAX_ACTIVE_INVITATIONS} active invitations. Revoke \
             one first."
        ),
        (
            ErrorCode::NotFound,
            ActionKind::SetMemberRole(_) | ActionKind::SetMemberDeactivated(_),
        ) => "That person is no longer in the workspace.".to_owned(),
        (ErrorCode::NotFound, ActionKind::RevokeInvitation(_)) => {
            "That invitation no longer exists.".to_owned()
        }
        (ErrorCode::Forbidden, ActionKind::UpdateWorkspace) => {
            said.unwrap_or_else(|| "Only admins and owners can change the workspace.".to_owned())
        }
        (ErrorCode::Forbidden | ErrorCode::Conflict | ErrorCode::InvalidRequest, _) => {
            said.unwrap_or_else(|| readable_error(error))
        }
        _ => readable_error(error),
    }
}
