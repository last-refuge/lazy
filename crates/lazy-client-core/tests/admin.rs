//! Workspace administration (slice 3F, D22): the workspace's name and settings, roles,
//! deactivation, invitations, registering with a code, and losing access.

mod common;

use common::{Driver, World, seq, user};
use lazy_client_core::{
    ActionKind, ActionOutcome, ActionState, Change, Effect, Input, LoadStatus, Output,
    QuickSwitcherTarget, RequestError, SessionPhase, StartupState,
};
use lazy_domain::workspace::{
    ChannelCreationPolicy, MAX_INVITE_TTL_SECS, MAX_INVITE_USES, MIN_INVITE_TTL_SECS,
    RegistrationPolicy, WorkspaceSettings,
};
use lazy_domain::{ChannelVisibility, InvitationId, Timestamp, UserId, WorkspaceRole};
use lazy_protocol::{
    ApiError, CreatedInvitation, ErrorCode, Event, Invitation, InvitationList, InvitationState,
    InviteCode, PeoplePage, Person, PersonResponse, ServerFrame, User, Workspace,
};

// ----- Helpers -----

struct Fx {
    world: World,
    driver: Driver,
    head: i64,
}

fn setup_as(role: WorkspaceRole) -> Fx {
    let mut world = World::new();
    world.role = role;
    setup_with(world)
}

fn setup_with(world: World) -> Fx {
    let mut driver = Driver::ready(&world);
    driver.load_history(world.channel.id, vec![], false);
    let head = world.head.get();
    Fx {
        world,
        driver,
        head,
    }
}

impl Fx {
    fn event(&mut self, event: Event) -> Output {
        self.head += 1;
        self.driver.send(self.world.event_at(self.head, event))
    }

    fn answer(&mut self, kind: ActionKind, result: Result<ActionOutcome, RequestError>) -> Output {
        self.driver.send(Input::ActionResult { kind, result })
    }

    fn state(&self, kind: ActionKind) -> ActionState {
        self.driver.core.action(kind).clone()
    }
}

fn rejected(code: ErrorCode, message: &str) -> RequestError {
    RequestError::Rejected(ApiError::new(code, message))
}

fn lost() -> RequestError {
    RequestError::Unknown("timed out".into())
}

fn person(user: &User, role: WorkspaceRole) -> Person {
    Person {
        user: user.clone(),
        role,
        joined_at: Timestamp::now(),
    }
}

fn invitation(workspace: &Workspace, by: &User, state: InvitationState) -> Invitation {
    Invitation {
        id: InvitationId::generate(),
        workspace_id: workspace.id,
        created_by: by.id,
        created_at: Timestamp::now(),
        expires_at: Timestamp::now(),
        max_uses: None,
        uses: 0,
        state,
    }
}

const CODE: &str = "Zx9-secret_code_value_0123456789";

fn created(workspace: &Workspace, by: &User) -> CreatedInvitation {
    CreatedInvitation {
        invitation: invitation(workspace, by, InvitationState::Active),
        code: InviteCode::new(CODE),
    }
}

fn generation_of(out: &Output) -> u64 {
    out.effects
        .iter()
        .find_map(|e| match e {
            Effect::LoadInvitations { generation, .. } => Some(*generation),
            _ => None,
        })
        .unwrap_or_else(|| panic!("invitations were requested in {:?}", out.effects))
}

fn people_generation(out: &Output) -> u64 {
    out.effects
        .iter()
        .find_map(|e| match e {
            Effect::LoadPeople { generation, .. } => Some(*generation),
            _ => None,
        })
        .unwrap_or_else(|| panic!("people were requested in {:?}", out.effects))
}

/// Opens the invitations and answers with `items`.
fn open_invitations(fx: &mut Fx, items: Vec<Invitation>) {
    let out = fx.driver.send(Input::OpenInvitations);
    let generation = generation_of(&out);
    fx.driver.send(Input::InvitationsLoaded {
        generation,
        result: Ok(InvitationList { invitations: items }),
    });
}

/// Opens the people directory and answers with `people`.
fn open_people(fx: &mut Fx, people: Vec<Person>) -> u64 {
    let out = fx.driver.send(Input::OpenPeople);
    let generation = people_generation(&out);
    fx.driver.send(Input::PeopleLoaded {
        generation,
        result: Ok(PeoplePage {
            people,
            has_more: false,
        }),
    });
    generation
}

/// Whether the quick switcher offers messaging `user` when `amy` is typed.
fn offers_person(fx: &Fx, user: UserId) -> bool {
    fx.driver
        .core
        .quick_switcher_results("amy")
        .iter()
        .any(|item| item.target == QuickSwitcherTarget::Person(user))
}

fn requests_of(out: &Output, matches: impl Fn(&Effect) -> bool) -> usize {
    out.effects.iter().filter(|e| matches(e)).count()
}

fn no_requests(out: &Output) -> bool {
    !out.effects.iter().any(|e| {
        !matches!(
            e,
            Effect::PersistCache(_) | Effect::CancelInvitations | Effect::CancelPeople
        )
    })
}

// ----- What the person may do -----

#[test]
fn a_new_core_offers_nothing_and_knows_default_settings() {
    let core = lazy_client_core::ClientCore::new();
    assert_eq!(core.my_role(), None);
    assert!(!core.can_manage_workspace());
    assert!(!core.can_create_channel());
    assert_eq!(core.workspace_settings(), WorkspaceSettings::default());
    assert_eq!(core.access_lost(), None);
    assert!(!core.invitations().open);
}

#[test]
fn what_is_offered_follows_the_role_and_the_channel_policy() {
    for (role, manages) in [
        (WorkspaceRole::Member, false),
        (WorkspaceRole::Admin, true),
        (WorkspaceRole::Owner, true),
    ] {
        let fx = setup_as(role);
        assert_eq!(fx.driver.core.my_role(), Some(role));
        assert_eq!(fx.driver.core.can_manage_workspace(), manages);
        assert!(fx.driver.core.can_create_channel(), "everyone, by default");

        let mut world = World::new();
        world.role = role;
        world.workspace.settings.channel_creation = ChannelCreationPolicy::AdminsOnly;
        let fx = setup_with(world);
        assert_eq!(fx.driver.core.can_create_channel(), manages);
        assert_eq!(
            fx.driver.core.workspace_settings().channel_creation,
            ChannelCreationPolicy::AdminsOnly
        );
    }
}

#[test]
fn a_channel_is_not_requested_when_the_policy_keeps_it_to_admins() {
    let mut world = World::new();
    world.workspace.settings.channel_creation = ChannelCreationPolicy::AdminsOnly;
    let mut fx = setup_with(world);
    let out = fx.driver.send(Input::CreateChannel {
        name: "ideas".into(),
        visibility: ChannelVisibility::Public,
        topic: String::new(),
        description: String::new(),
    });
    assert!(no_requests(&out), "no request: {:?}", out.effects);
    assert!(matches!(
        fx.state(ActionKind::CreateChannel),
        ActionState::Failed(text) if text.contains("admins and owners")
    ));
}

// ----- The workspace's name and settings -----

#[test]
fn a_name_is_checked_here_and_one_request_carries_the_change() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    let settings = WorkspaceSettings {
        registration: RegistrationPolicy::InviteOnly,
        channel_creation: ChannelCreationPolicy::AdminsOnly,
    };
    let out = fx.driver.send(Input::UpdateWorkspace {
        name: Some("  Acme Inc  ".into()),
        settings: Some(settings),
    });
    assert_eq!(fx.state(ActionKind::UpdateWorkspace), ActionState::InFlight);
    let [Effect::UpdateWorkspace { workspace, request }] = out
        .effects
        .iter()
        .filter(|e| matches!(e, Effect::UpdateWorkspace { .. }))
        .collect::<Vec<_>>()[..]
    else {
        panic!("one request: {:?}", out.effects);
    };
    assert_eq!(*workspace, fx.world.workspace.id);
    assert_eq!(request.name.as_ref().unwrap().as_str(), "Acme Inc");
    assert_eq!(request.settings, Some(settings));

    // A second press while it is out is ignored.
    let out = fx.driver.send(Input::UpdateWorkspace {
        name: Some("Other".into()),
        settings: None,
    });
    assert!(no_requests(&out));
}

#[test]
fn a_blank_or_long_name_and_an_empty_change_send_nothing() {
    let mut fx = setup_as(WorkspaceRole::Owner);
    for name in ["", "   ", &"x".repeat(65)] {
        let out = fx.driver.send(Input::UpdateWorkspace {
            name: Some(name.into()),
            settings: None,
        });
        assert!(no_requests(&out), "{name:?}");
        assert!(matches!(
            fx.state(ActionKind::UpdateWorkspace),
            ActionState::Failed(_)
        ));
    }
    // Changing nothing is nothing, not an error.
    fx.driver
        .send(Input::DismissAction(ActionKind::UpdateWorkspace));
    let out = fx.driver.send(Input::UpdateWorkspace {
        name: None,
        settings: None,
    });
    assert!(no_requests(&out));
    assert_eq!(fx.state(ActionKind::UpdateWorkspace), ActionState::Idle);
}

#[test]
fn a_member_cannot_change_the_workspace() {
    let mut fx = setup_as(WorkspaceRole::Member);
    let out = fx.driver.send(Input::UpdateWorkspace {
        name: Some("Mine now".into()),
        settings: None,
    });
    assert!(no_requests(&out));
    assert!(matches!(
        fx.state(ActionKind::UpdateWorkspace),
        ActionState::Failed(_)
    ));
}

#[test]
fn the_answer_replaces_the_name_and_settings_and_is_cached() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    fx.driver.send(Input::UpdateWorkspace {
        name: Some("Acme".into()),
        settings: None,
    });
    let mut workspace = fx.world.workspace.clone();
    workspace.name = "Acme".into();
    workspace.settings.registration = RegistrationPolicy::InviteOnly;
    let out = fx.answer(
        ActionKind::UpdateWorkspace,
        Ok(ActionOutcome::Workspace(workspace.clone())),
    );
    assert!(out.changes.contains(&Change::WorkspaceChanged));
    let held = fx.driver.core.primary_workspace().unwrap();
    assert_eq!(held.workspace, workspace);
    assert_eq!(held.role, WorkspaceRole::Admin);
    assert_eq!(
        fx.driver.core.workspace_settings().registration,
        RegistrationPolicy::InviteOnly
    );
    assert_eq!(fx.state(ActionKind::UpdateWorkspace), ActionState::Idle);
    let cache = fx.driver.last_cache().expect("saved");
    assert_eq!(cache.workspaces[0].workspace, workspace);
}

#[test]
fn a_refusal_keeps_what_was_shown_and_says_why() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    fx.driver.send(Input::UpdateWorkspace {
        name: Some("Acme".into()),
        settings: None,
    });
    let out = fx.answer(
        ActionKind::UpdateWorkspace,
        Err(rejected(
            ErrorCode::Forbidden,
            "admins only, and you are not",
        )),
    );
    assert!(!out.changes.contains(&Change::WorkspaceChanged));
    assert_eq!(
        fx.driver.core.primary_workspace().unwrap().workspace.name,
        "Lazy"
    );
    assert_eq!(
        fx.state(ActionKind::UpdateWorkspace),
        ActionState::Failed("admins only, and you are not".into())
    );
    // Trying again is a new request.
    let out = fx.driver.send(Input::UpdateWorkspace {
        name: Some("Acme".into()),
        settings: None,
    });
    assert_eq!(
        requests_of(&out, |e| matches!(e, Effect::UpdateWorkspace { .. })),
        1
    );
}

#[test]
fn an_event_updates_the_name_and_settings_for_everyone() {
    let mut fx = setup_as(WorkspaceRole::Member);
    let mut workspace = fx.world.workspace.clone();
    workspace.name = "Renamed".into();
    workspace.settings.channel_creation = ChannelCreationPolicy::AdminsOnly;
    let out = fx.event(Event::WorkspaceUpdated {
        workspace: workspace.clone(),
    });
    assert!(out.changes.contains(&Change::WorkspaceChanged));
    assert_eq!(
        fx.driver.core.primary_workspace().unwrap().workspace,
        workspace
    );
    assert!(
        !fx.driver.core.can_create_channel(),
        "the policy applies at once"
    );
    // The same again says nothing.
    let out = fx.event(Event::WorkspaceUpdated { workspace });
    assert!(!out.changes.contains(&Change::WorkspaceChanged));
}

// ----- Roles -----

#[test]
fn an_admin_sets_a_members_role_and_the_loaded_person_shows_it() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    let zoe = fx.world.other.clone();
    open_people(&mut fx, vec![person(&zoe, WorkspaceRole::Member)]);
    let kind = ActionKind::SetMemberRole(zoe.id);
    let out = fx.driver.send(Input::SetMemberRole {
        user_id: zoe.id,
        role: WorkspaceRole::Admin,
    });
    assert!(out.effects.iter().any(|e| matches!(
        e,
        Effect::SetMemberRole { user, request, .. }
            if *user == zoe.id && request.role == WorkspaceRole::Admin
    )));
    assert_eq!(fx.state(kind), ActionState::InFlight);
    let out = fx.answer(
        kind,
        Ok(ActionOutcome::Role {
            user: zoe.id,
            role: WorkspaceRole::Admin,
        }),
    );
    assert!(out.changes.contains(&Change::PeopleChanged));
    assert_eq!(fx.driver.core.people().people[0].role, WorkspaceRole::Admin);
    assert_eq!(fx.state(kind), ActionState::Idle);
    // Someone else's role is not mine.
    assert_eq!(fx.driver.core.my_role(), Some(WorkspaceRole::Admin));
}

#[test]
fn what_cannot_work_is_not_asked_for() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    let owner = user("olive");
    let zoe = fx.world.other.clone();
    open_people(
        &mut fx,
        vec![
            person(&owner, WorkspaceRole::Owner),
            person(&zoe, WorkspaceRole::Member),
        ],
    );
    // An admin neither touches an owner nor makes one.
    for (target, role) in [
        (owner.id, WorkspaceRole::Member),
        (zoe.id, WorkspaceRole::Owner),
    ] {
        let out = fx.driver.send(Input::SetMemberRole {
            user_id: target,
            role,
        });
        assert!(no_requests(&out));
        assert!(matches!(
            fx.state(ActionKind::SetMemberRole(target)),
            ActionState::Failed(_)
        ));
    }
    // A member changes nobody's role.
    let mut fx = setup_as(WorkspaceRole::Member);
    let out = fx.driver.send(Input::SetMemberRole {
        user_id: fx.world.other.id,
        role: WorkspaceRole::Admin,
    });
    assert!(no_requests(&out));
}

#[test]
fn an_owner_may_make_an_owner_and_the_last_owner_rule_is_the_servers() {
    let mut fx = setup_as(WorkspaceRole::Owner);
    let me = fx.world.me.clone();
    open_people(&mut fx, vec![person(&me, WorkspaceRole::Owner)]);
    // The core does not count owners (the server does, under its lock): it asks.
    let out = fx.driver.send(Input::SetMemberRole {
        user_id: me.id,
        role: WorkspaceRole::Member,
    });
    assert_eq!(
        requests_of(&out, |e| matches!(e, Effect::SetMemberRole { .. })),
        1
    );
    let kind = ActionKind::SetMemberRole(me.id);
    fx.answer(
        kind,
        Err(rejected(
            ErrorCode::Conflict,
            "a workspace needs at least one owner",
        )),
    );
    assert_eq!(
        fx.state(kind),
        ActionState::Failed("a workspace needs at least one owner".into())
    );
    assert_eq!(fx.driver.core.my_role(), Some(WorkspaceRole::Owner));
    assert_eq!(fx.driver.core.people().people[0].role, WorkspaceRole::Owner);
}

#[test]
fn role_changes_of_different_people_are_separate_actions() {
    let mut fx = setup_as(WorkspaceRole::Owner);
    let zoe = fx.world.other.clone();
    let amy = user("amy");
    for target in [zoe.id, amy.id] {
        let out = fx.driver.send(Input::SetMemberRole {
            user_id: target,
            role: WorkspaceRole::Admin,
        });
        assert_eq!(
            requests_of(&out, |e| matches!(e, Effect::SetMemberRole { .. })),
            1
        );
    }
    // The same person twice is one request.
    let out = fx.driver.send(Input::SetMemberRole {
        user_id: zoe.id,
        role: WorkspaceRole::Member,
    });
    assert!(no_requests(&out));
    fx.answer(
        ActionKind::SetMemberRole(zoe.id),
        Ok(ActionOutcome::Role {
            user: zoe.id,
            role: WorkspaceRole::Admin,
        }),
    );
    assert_eq!(
        fx.state(ActionKind::SetMemberRole(zoe.id)),
        ActionState::Idle
    );
    assert_eq!(
        fx.state(ActionKind::SetMemberRole(amy.id)),
        ActionState::InFlight
    );
}

#[test]
fn a_role_event_updates_the_directory_and_the_profile() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    let zoe = fx.world.other.clone();
    open_people(&mut fx, vec![person(&zoe, WorkspaceRole::Member)]);
    let out = fx.driver.send(Input::OpenProfile { user_id: zoe.id });
    let generation = out
        .effects
        .iter()
        .find_map(|e| match e {
            Effect::LoadProfile { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("profile asked");
    fx.driver.send(Input::ProfileLoaded {
        generation,
        result: Ok(PersonResponse {
            person: person(&zoe, WorkspaceRole::Member),
        }),
    });
    let out = fx.event(Event::MemberRoleChanged {
        user_id: zoe.id,
        role: WorkspaceRole::Admin,
    });
    assert!(out.changes.contains(&Change::PeopleChanged));
    assert!(out.changes.contains(&Change::ProfileChanged));
    assert_eq!(fx.driver.core.people().people[0].role, WorkspaceRole::Admin);
    assert_eq!(
        fx.driver
            .core
            .profile()
            .unwrap()
            .person
            .as_ref()
            .unwrap()
            .role,
        WorkspaceRole::Admin
    );
    assert_eq!(fx.driver.core.my_role(), Some(WorkspaceRole::Admin));
}

#[test]
fn my_own_role_changing_changes_what_is_offered_and_is_cached() {
    let mut fx = setup_as(WorkspaceRole::Member);
    let me = fx.world.me.id;
    let out = fx.event(Event::MemberRoleChanged {
        user_id: me,
        role: WorkspaceRole::Admin,
    });
    assert!(out.changes.contains(&Change::WorkspaceChanged));
    assert!(fx.driver.core.can_manage_workspace());
    assert_eq!(
        fx.driver.last_cache().unwrap().workspaces[0].role,
        WorkspaceRole::Admin
    );
}

#[test]
fn losing_the_role_closes_the_invitations_and_stops_listing_deactivated_people() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    let me = fx.world.me.id;
    open_people(&mut fx, vec![]);
    fx.driver.send(Input::SetPeopleIncludeDeactivated(true));
    assert!(fx.driver.core.people().include_deactivated);
    open_invitations(&mut fx, vec![]);
    fx.driver.send(Input::CreateInvitation {
        expires_in_secs: None,
        max_uses: None,
    });
    let workspace = fx.world.workspace.clone();
    let admin = fx.world.me.clone();
    fx.answer(
        ActionKind::CreateInvitation,
        Ok(ActionOutcome::InvitationCreated(created(
            &workspace, &admin,
        ))),
    );
    assert!(fx.driver.core.invitations().created.is_some());

    let out = fx.event(Event::MemberRoleChanged {
        user_id: me,
        role: WorkspaceRole::Member,
    });
    assert!(!fx.driver.core.invitations().open);
    assert!(
        fx.driver.core.invitations().created.is_none(),
        "the code goes"
    );
    assert!(!fx.driver.core.people().include_deactivated);
    assert_eq!(
        requests_of(&out, |e| matches!(e, Effect::LoadPeople { query, .. }
            if query.include_deactivated.is_none())),
        1,
        "the directory is asked again without them"
    );
}

// ----- Deactivation -----

#[test]
fn deactivating_someone_updates_the_directory_that_leaves_them_out() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    let zoe = fx.world.other.clone();
    open_people(&mut fx, vec![person(&zoe, WorkspaceRole::Member)]);
    let kind = ActionKind::SetMemberDeactivated(zoe.id);
    let out = fx.driver.send(Input::SetMemberDeactivated {
        user_id: zoe.id,
        deactivated: true,
    });
    assert!(out.effects.iter().any(|e| matches!(
        e,
        Effect::SetMemberDeactivated { user, request, .. } if *user == zoe.id && request.deactivated
    )));
    fx.answer(
        kind,
        Ok(ActionOutcome::Deactivated {
            user: zoe.id,
            deactivated: true,
        }),
    );
    assert!(fx.driver.core.member(zoe.id).unwrap().deactivated);
    assert!(fx.driver.core.people().people.is_empty());
    assert_eq!(fx.state(kind), ActionState::Idle);
}

#[test]
fn nobody_deactivates_themself_and_an_admin_leaves_admins_and_owners_alone() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    let me = fx.world.me.id;
    let out = fx.driver.send(Input::SetMemberDeactivated {
        user_id: me,
        deactivated: true,
    });
    assert!(no_requests(&out));
    assert!(matches!(
        fx.state(ActionKind::SetMemberDeactivated(me)),
        ActionState::Failed(_)
    ));

    let owner = user("olive");
    let peer = user("pat");
    open_people(
        &mut fx,
        vec![
            person(&owner, WorkspaceRole::Owner),
            person(&peer, WorkspaceRole::Admin),
        ],
    );
    for target in [owner.id, peer.id] {
        let out = fx.driver.send(Input::SetMemberDeactivated {
            user_id: target,
            deactivated: true,
        });
        assert!(no_requests(&out));
    }
    // An owner may act on an admin.
    let mut fx = setup_as(WorkspaceRole::Owner);
    open_people(&mut fx, vec![person(&peer, WorkspaceRole::Admin)]);
    let out = fx.driver.send(Input::SetMemberDeactivated {
        user_id: peer.id,
        deactivated: true,
    });
    assert_eq!(
        requests_of(&out, |e| matches!(e, Effect::SetMemberDeactivated { .. })),
        1
    );
    // A member does nothing.
    let mut fx = setup_as(WorkspaceRole::Member);
    let out = fx.driver.send(Input::SetMemberDeactivated {
        user_id: peer.id,
        deactivated: true,
    });
    assert!(no_requests(&out));
}

#[test]
fn a_refused_deactivation_changes_nothing_and_says_why() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    let zoe = fx.world.other.clone();
    open_people(&mut fx, vec![person(&zoe, WorkspaceRole::Member)]);
    let kind = ActionKind::SetMemberDeactivated(zoe.id);
    fx.driver.send(Input::SetMemberDeactivated {
        user_id: zoe.id,
        deactivated: true,
    });
    fx.answer(
        kind,
        Err(rejected(ErrorCode::Forbidden, "you cannot do this to them")),
    );
    assert!(!fx.driver.core.member(zoe.id).unwrap().deactivated);
    assert_eq!(fx.driver.core.people().people.len(), 1);
    assert_eq!(
        fx.state(kind),
        ActionState::Failed("you cannot do this to them".into())
    );
    // The same is said of an unknown outcome, in plain words.
    fx.driver.send(Input::SetMemberDeactivated {
        user_id: zoe.id,
        deactivated: true,
    });
    fx.answer(kind, Err(lost()));
    assert!(matches!(fx.state(kind), ActionState::Failed(text) if text.contains("reach")));
}

#[test]
fn a_deactivated_person_is_no_longer_offered_but_still_has_a_name() {
    let mut world = World::new();
    let amy = user("amy");
    world.extra_users.push(amy.clone());
    let mut fx = setup_with(world);
    let general = fx.world.channel.id;
    assert!(fx.driver.core.members().iter().any(|u| u.id == amy.id));
    assert!(offers_person(&fx, amy.id));

    let out = fx.event(Event::MemberDeactivated {
        user_id: amy.id,
        deactivated: true,
    });
    assert!(out.changes.contains(&Change::DirectoryChanged));
    assert!(!fx.driver.core.members().iter().any(|u| u.id == amy.id));
    assert!(!offers_person(&fx, amy.id));
    // Her old messages still show her name.
    assert_eq!(fx.driver.core.display_name(amy.id), "AMY");
    assert!(fx.driver.core.member(amy.id).unwrap().deactivated);

    // She cannot be messaged or added.
    let out = fx.driver.send(Input::OpenConversation {
        participants: vec![amy.id],
    });
    assert!(no_requests(&out));
    assert!(matches!(
        fx.state(ActionKind::OpenConversation),
        ActionState::Failed(text) if text.contains("Deactivated")
    ));
    let out = fx.driver.send(Input::AddMembers {
        id: general,
        users: vec![amy.id],
    });
    assert!(no_requests(&out));
    assert!(matches!(
        fx.state(ActionKind::AddMembers(general)),
        ActionState::Failed(_)
    ));

    // Reactivating brings her back.
    fx.event(Event::MemberDeactivated {
        user_id: amy.id,
        deactivated: false,
    });
    assert!(fx.driver.core.members().iter().any(|u| u.id == amy.id));
    assert!(offers_person(&fx, amy.id));
}

#[test]
fn a_deactivated_person_is_not_a_mention_candidate() {
    let mut world = World::new();
    let amy = user("amy");
    world.extra_users.push(amy.clone());
    let mut fx = setup_with(world);
    let general = fx.world.channel.id;
    let generation = fx
        .driver
        .effects_matching(|e| match e {
            Effect::FetchMembers {
                conversation,
                generation,
            } if *conversation == general => Some(*generation),
            _ => None,
        })
        .pop()
        .expect("members asked");
    fx.driver.send(Input::MembersFetched {
        conversation: general,
        generation,
        result: Ok(lazy_protocol::ConversationMembersResponse {
            conversation_id: general,
            user_ids: vec![fx.world.me.id, fx.world.other.id, amy.id],
        }),
    });
    let handles = |fx: &Fx| -> Vec<String> {
        fx.driver
            .core
            .mention_candidates(general, "")
            .iter()
            .map(|c| c.handle.clone())
            .collect()
    };
    assert!(handles(&fx).contains(&"amy".to_owned()));
    fx.event(Event::MemberDeactivated {
        user_id: amy.id,
        deactivated: true,
    });
    assert!(!handles(&fx).contains(&"amy".to_owned()));
    fx.event(Event::MemberDeactivated {
        user_id: amy.id,
        deactivated: false,
    });
    assert!(handles(&fx).contains(&"amy".to_owned()));
}

#[test]
fn deactivated_people_can_stay_in_a_group_but_cannot_be_added_to_one() {
    let mut world = World::new();
    world.role = WorkspaceRole::Admin;
    let amy = user("amy");
    let bob = user("bob");
    world.extra_users.push(amy.clone());
    world.extra_users.push(bob.clone());
    let group = lazy_protocol::UserGroup {
        id: lazy_domain::GroupId::generate(),
        workspace_id: world.workspace.id,
        handle: lazy_domain::GroupHandle::parse("team").unwrap(),
        name: lazy_domain::DisplayName::parse("Team").unwrap(),
        members: vec![amy.id],
    };
    world.groups.push(group.clone());
    let mut fx = setup_with(world);
    fx.event(Event::MemberDeactivated {
        user_id: amy.id,
        deactivated: true,
    });
    fx.event(Event::MemberDeactivated {
        user_id: bob.id,
        deactivated: true,
    });
    // Renaming the group keeps its full member list, a deactivated one included.
    let out = fx.driver.send(Input::UpdateGroup {
        id: group.id,
        handle: None,
        name: Some("Squad".into()),
        members: Some(vec![amy.id]),
    });
    assert_eq!(
        requests_of(&out, |e| matches!(e, Effect::UpdateGroup { .. })),
        1
    );
    // Adding another deactivated person is refused.
    let out = fx.driver.send(Input::CreateGroup {
        handle: "crew".into(),
        name: "Crew".into(),
        members: vec![bob.id],
    });
    assert!(no_requests(&out));
    assert!(matches!(
        fx.state(ActionKind::CreateGroup),
        ActionState::Failed(text) if text.contains("Deactivated")
    ));
}

#[test]
fn the_directory_can_list_deactivated_people_for_admins_only() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    let mut zoe = fx.world.other.clone();
    open_people(&mut fx, vec![person(&zoe, WorkspaceRole::Member)]);
    let out = fx.driver.send(Input::SetPeopleIncludeDeactivated(true));
    let generation = people_generation(&out);
    assert!(out.effects.iter().any(|e| matches!(
        e,
        Effect::LoadPeople { query, .. }
            if query.include_deactivated == Some(true) && query.offset.is_none()
    )));
    zoe.deactivated = true;
    fx.driver.send(Input::PeopleLoaded {
        generation,
        result: Ok(PeoplePage {
            people: vec![person(&zoe, WorkspaceRole::Member)],
            has_more: false,
        }),
    });
    assert!(fx.driver.core.people().people[0].user.deactivated);

    // A deactivation event keeps a row the list includes, marked.
    let amy = user("amy");
    fx.event(Event::MemberJoined { user: amy.clone() });
    fx.event(Event::MemberDeactivated {
        user_id: zoe.id,
        deactivated: false,
    });
    assert_eq!(fx.driver.core.people().people.len(), 1);
    assert!(!fx.driver.core.people().people[0].user.deactivated);

    // A member's directory never asks for them.
    let mut fx = setup_as(WorkspaceRole::Member);
    open_people(&mut fx, vec![]);
    let out = fx.driver.send(Input::SetPeopleIncludeDeactivated(true));
    assert!(no_requests(&out));
    assert!(!fx.driver.core.people().include_deactivated);
}

#[test]
fn the_flag_belongs_to_the_open_directory_and_closing_forgets_it() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    // Sent before the directory is open, it is ignored (opening starts from the defaults).
    let out = fx.driver.send(Input::SetPeopleIncludeDeactivated(true));
    assert!(no_requests(&out));
    let out = fx.driver.send(Input::OpenPeople);
    assert!(out.effects.iter().any(|e| matches!(
        e,
        Effect::LoadPeople { query, .. } if query.include_deactivated.is_none()
    )));
    assert!(!fx.driver.core.people().include_deactivated);
    fx.driver.send(Input::SetPeopleIncludeDeactivated(true));
    assert!(fx.driver.core.people().include_deactivated);
    // Closing and opening again starts from the defaults once more.
    fx.driver.send(Input::ClosePeople);
    assert!(!fx.driver.core.people().include_deactivated);
    let out = fx.driver.send(Input::OpenPeople);
    assert!(out.effects.iter().any(|e| matches!(
        e,
        Effect::LoadPeople { query, .. } if query.include_deactivated.is_none()
    )));
}

#[test]
fn a_reactivated_person_missing_from_the_directory_asks_it_again() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    let zoe = fx.world.other.clone();
    open_people(&mut fx, vec![]);
    let out = fx.event(Event::MemberDeactivated {
        user_id: zoe.id,
        deactivated: false,
    });
    assert_eq!(
        requests_of(&out, |e| matches!(e, Effect::LoadPeople { query, .. }
            if query.offset.is_none())),
        1
    );
}

// ----- Invitations -----

#[test]
fn managers_open_the_invitations_and_members_do_not() {
    let mut fx = setup_as(WorkspaceRole::Member);
    let out = fx.driver.send(Input::OpenInvitations);
    assert!(no_requests(&out));
    assert!(!fx.driver.core.invitations().open);

    let mut fx = setup_as(WorkspaceRole::Admin);
    let admin = fx.world.me.clone();
    let workspace = fx.world.workspace.clone();
    let out = fx.driver.send(Input::OpenInvitations);
    let generation = generation_of(&out);
    assert!(fx.driver.core.invitations().open);
    assert_eq!(fx.driver.core.invitations().status, LoadStatus::Loading);
    let items = vec![
        invitation(&workspace, &admin, InvitationState::Active),
        invitation(&workspace, &admin, InvitationState::Revoked),
    ];
    fx.driver.send(Input::InvitationsLoaded {
        generation,
        result: Ok(InvitationList {
            invitations: items.clone(),
        }),
    });
    assert_eq!(fx.driver.core.invitations().items, items);
    assert_eq!(fx.driver.core.invitations().status, LoadStatus::Ready);
}

#[test]
fn an_old_answer_and_a_closed_view_are_ignored() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    let admin = fx.world.me.clone();
    let workspace = fx.world.workspace.clone();
    let out = fx.driver.send(Input::OpenInvitations);
    let first = generation_of(&out);
    fx.driver.send(Input::CloseInvitations);
    let out = fx.driver.send(Input::OpenInvitations);
    let second = generation_of(&out);
    assert_ne!(first, second);
    let late = invitation(&workspace, &admin, InvitationState::Active);
    fx.driver.send(Input::InvitationsLoaded {
        generation: first,
        result: Ok(InvitationList {
            invitations: vec![late],
        }),
    });
    assert!(fx.driver.core.invitations().items.is_empty());
    assert_eq!(fx.driver.core.invitations().status, LoadStatus::Loading);
    fx.driver.send(Input::CloseInvitations);
    fx.driver.send(Input::InvitationsLoaded {
        generation: second,
        result: Ok(InvitationList {
            invitations: vec![],
        }),
    });
    assert!(!fx.driver.core.invitations().open);
}

#[test]
fn a_failed_list_says_why_and_opening_again_asks_again() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    let out = fx.driver.send(Input::OpenInvitations);
    let generation = generation_of(&out);
    fx.driver.send(Input::InvitationsLoaded {
        generation,
        result: Err(lost()),
    });
    assert!(matches!(
        fx.driver.core.invitations().status,
        LoadStatus::Failed {
            can_retry: true,
            ..
        }
    ));
    let out = fx.driver.send(Input::OpenInvitations);
    let generation = generation_of(&out);
    fx.driver.send(Input::InvitationsLoaded {
        generation,
        result: Err(rejected(ErrorCode::Forbidden, "no")),
    });
    assert!(matches!(
        &fx.driver.core.invitations().status,
        LoadStatus::Failed { reason, can_retry: false } if reason.contains("admins")
    ));
}

#[test]
fn an_invitation_is_checked_here_before_it_is_asked_for() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    open_invitations(&mut fx, vec![]);
    for (secs, uses) in [
        (Some(MIN_INVITE_TTL_SECS - 1), None),
        (Some(MAX_INVITE_TTL_SECS + 1), None),
        (Some(0), None),
        (None, Some(0)),
        (None, Some(MAX_INVITE_USES + 1)),
    ] {
        let out = fx.driver.send(Input::CreateInvitation {
            expires_in_secs: secs,
            max_uses: uses,
        });
        assert!(no_requests(&out), "{secs:?} {uses:?}");
        assert!(matches!(
            fx.state(ActionKind::CreateInvitation),
            ActionState::Failed(_)
        ));
    }
    let out = fx.driver.send(Input::CreateInvitation {
        expires_in_secs: Some(MIN_INVITE_TTL_SECS),
        max_uses: Some(MAX_INVITE_USES),
    });
    assert!(out.effects.iter().any(|e| matches!(
        e,
        Effect::CreateInvitation { request, .. }
            if request.expires_in_secs == Some(MIN_INVITE_TTL_SECS)
                && request.max_uses == Some(MAX_INVITE_USES)
    )));
    // A member makes none; an unopened view makes none.
    let mut fx = setup_as(WorkspaceRole::Admin);
    let out = fx.driver.send(Input::CreateInvitation {
        expires_in_secs: None,
        max_uses: None,
    });
    assert!(no_requests(&out));
}

#[test]
fn the_code_is_shown_until_dismissed_and_nowhere_else() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    let admin = fx.world.me.clone();
    let workspace = fx.world.workspace.clone();
    open_invitations(&mut fx, vec![]);
    fx.driver.send(Input::CreateInvitation {
        expires_in_secs: None,
        max_uses: Some(3),
    });
    let made = created(&workspace, &admin);
    let out = fx.answer(
        ActionKind::CreateInvitation,
        Ok(ActionOutcome::InvitationCreated(made.clone())),
    );
    assert!(out.changes.contains(&Change::InvitationsChanged));
    let view = fx.driver.core.invitations();
    assert_eq!(view.items[0], made.invitation, "newest first");
    let shown = view.created.as_ref().expect("shown");
    assert_eq!(shown.code.expose(), CODE);
    assert_eq!(shown.invitation, made.invitation);

    // Nothing the core prints, caches, or asks for holds it.
    assert!(!format!("{:?}", fx.driver.core.invitations()).contains(CODE));
    assert!(!format!("{out:?}").contains(CODE));
    for cache in fx.driver.caches() {
        assert!(!serde_json::to_string(cache).unwrap().contains(CODE));
    }
    assert!(!format!("{:?}", fx.driver.effects).contains(CODE));

    // Making another replaces it; dismissing forgets it and keeps the list.
    fx.driver.send(Input::DismissCreatedInvitation);
    assert!(fx.driver.core.invitations().created.is_none());
    assert_eq!(fx.driver.core.invitations().items.len(), 1);
}

#[test]
fn closing_signing_out_and_a_lost_session_forget_the_code() {
    for how in ["close", "sign out", "expired", "demoted"] {
        let mut fx = setup_as(WorkspaceRole::Admin);
        let admin = fx.world.me.clone();
        let workspace = fx.world.workspace.clone();
        open_invitations(&mut fx, vec![]);
        fx.driver.send(Input::CreateInvitation {
            expires_in_secs: None,
            max_uses: None,
        });
        fx.answer(
            ActionKind::CreateInvitation,
            Ok(ActionOutcome::InvitationCreated(created(
                &workspace, &admin,
            ))),
        );
        assert!(fx.driver.core.invitations().created.is_some());
        match how {
            "close" => fx.driver.send(Input::CloseInvitations),
            "sign out" => fx.driver.send(Input::SignOut),
            "expired" => fx.driver.send(Input::SessionExpired),
            _ => fx.event(Event::MemberRoleChanged {
                user_id: admin.id,
                role: WorkspaceRole::Member,
            }),
        };
        assert!(fx.driver.core.invitations().created.is_none(), "{how}");
        assert!(!fx.driver.core.invitations().open, "{how}");
        assert!(fx.driver.core.invitations().items.is_empty(), "{how}");
    }
}

#[test]
fn an_answer_for_a_closed_view_drops_the_code() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    let admin = fx.world.me.clone();
    let workspace = fx.world.workspace.clone();
    open_invitations(&mut fx, vec![]);
    fx.driver.send(Input::CreateInvitation {
        expires_in_secs: None,
        max_uses: None,
    });
    fx.driver.send(Input::CloseInvitations);
    fx.answer(
        ActionKind::CreateInvitation,
        Ok(ActionOutcome::InvitationCreated(created(
            &workspace, &admin,
        ))),
    );
    assert!(fx.driver.core.invitations().created.is_none());
    assert!(!fx.driver.core.invitations().open);
}

#[test]
fn an_unknown_outcome_is_never_repeated_the_list_is_asked_and_the_person_told() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    open_invitations(&mut fx, vec![]);
    let out = fx.driver.send(Input::CreateInvitation {
        expires_in_secs: None,
        max_uses: None,
    });
    assert_eq!(
        requests_of(&out, |e| matches!(e, Effect::CreateInvitation { .. })),
        1
    );
    let out = fx.answer(ActionKind::CreateInvitation, Err(lost()));
    assert_eq!(
        requests_of(&out, |e| matches!(e, Effect::CreateInvitation { .. })),
        0,
        "never repeated by the core"
    );
    assert_eq!(
        requests_of(&out, |e| matches!(e, Effect::LoadInvitations { .. })),
        1
    );
    assert!(fx.driver.core.notice().is_some());
    assert!(matches!(
        fx.state(ActionKind::CreateInvitation),
        ActionState::Failed(_)
    ));
    // The person may make another on purpose.
    let out = fx.driver.send(Input::CreateInvitation {
        expires_in_secs: None,
        max_uses: None,
    });
    assert_eq!(
        requests_of(&out, |e| matches!(e, Effect::CreateInvitation { .. })),
        1
    );
}

#[test]
fn a_definite_refusal_to_create_says_why_and_does_not_reload() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    open_invitations(&mut fx, vec![]);
    fx.driver.send(Input::CreateInvitation {
        expires_in_secs: None,
        max_uses: None,
    });
    let out = fx.answer(
        ActionKind::CreateInvitation,
        Err(rejected(ErrorCode::LimitReached, "too many")),
    );
    assert_eq!(
        requests_of(&out, |e| matches!(e, Effect::LoadInvitations { .. })),
        0
    );
    assert!(matches!(
        fx.state(ActionKind::CreateInvitation),
        ActionState::Failed(text) if text.contains("100")
    ));
}

#[test]
fn revoking_updates_the_row_and_a_missing_one_says_so() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    let admin = fx.world.me.clone();
    let workspace = fx.world.workspace.clone();
    let active = invitation(&workspace, &admin, InvitationState::Active);
    open_invitations(&mut fx, vec![active.clone()]);
    let kind = ActionKind::RevokeInvitation(active.id);
    let out = fx.driver.send(Input::RevokeInvitation(active.id));
    assert!(
        out.effects.iter().any(
            |e| matches!(e, Effect::RevokeInvitation { invitation } if *invitation == active.id)
        )
    );
    let mut revoked = active.clone();
    revoked.state = InvitationState::Revoked;
    let out = fx.answer(kind, Ok(ActionOutcome::Invitation(revoked.clone())));
    assert!(out.changes.contains(&Change::InvitationsChanged));
    assert_eq!(fx.driver.core.invitations().items, vec![revoked]);
    assert_eq!(fx.state(kind), ActionState::Idle);

    fx.driver.send(Input::RevokeInvitation(active.id));
    fx.answer(kind, Err(rejected(ErrorCode::NotFound, "")));
    assert!(matches!(
        fx.state(kind),
        ActionState::Failed(text) if text.contains("no longer exists")
    ));
    // A member cannot.
    let mut fx = setup_as(WorkspaceRole::Member);
    let out = fx.driver.send(Input::RevokeInvitation(active.id));
    assert!(no_requests(&out));
}

#[test]
fn a_revocation_whose_answer_was_lost_asks_for_the_list_and_a_refusal_does_not() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    let admin = fx.world.me.clone();
    let workspace = fx.world.workspace.clone();
    let active = invitation(&workspace, &admin, InvitationState::Active);
    open_invitations(&mut fx, vec![active.clone()]);
    let kind = ActionKind::RevokeInvitation(active.id);

    // Nothing else tells the view whether it happened, so the list is asked for again.
    fx.driver.send(Input::RevokeInvitation(active.id));
    let out = fx.answer(kind, Err(lost()));
    assert_eq!(
        requests_of(&out, |e| matches!(e, Effect::LoadInvitations { .. })),
        1
    );
    assert!(matches!(fx.state(kind), ActionState::Failed(_)));
    let generation = generation_of(&out);
    let mut revoked = active.clone();
    revoked.state = InvitationState::Revoked;
    fx.driver.send(Input::InvitationsLoaded {
        generation,
        result: Ok(InvitationList {
            invitations: vec![revoked.clone()],
        }),
    });
    assert_eq!(fx.driver.core.invitations().items, vec![revoked]);

    // A refusal is final: the server did not act.
    fx.driver.send(Input::RevokeInvitation(active.id));
    let out = fx.answer(kind, Err(rejected(ErrorCode::Forbidden, "no")));
    assert_eq!(
        requests_of(&out, |e| matches!(e, Effect::LoadInvitations { .. })),
        0
    );
}

#[test]
fn revoking_the_invitation_just_created_updates_the_shown_one_too() {
    let mut fx = setup_as(WorkspaceRole::Admin);
    let admin = fx.world.me.clone();
    let workspace = fx.world.workspace.clone();
    open_invitations(&mut fx, vec![]);
    fx.driver.send(Input::CreateInvitation {
        expires_in_secs: None,
        max_uses: None,
    });
    let made = created(&workspace, &admin);
    fx.answer(
        ActionKind::CreateInvitation,
        Ok(ActionOutcome::InvitationCreated(made.clone())),
    );
    fx.driver.send(Input::RevokeInvitation(made.invitation.id));
    let mut revoked = made.invitation.clone();
    revoked.state = InvitationState::Revoked;
    fx.answer(
        ActionKind::RevokeInvitation(made.invitation.id),
        Ok(ActionOutcome::Invitation(revoked.clone())),
    );
    let shown = fx.driver.core.invitations().created.as_ref().unwrap();
    assert_eq!(shown.invitation, revoked);
    assert_eq!(shown.code.expose(), CODE, "the code stays until dismissed");
}

// ----- Registering with a code -----

fn register(code: &str) -> (Driver, Output) {
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState::default()));
    let out = driver.send(Input::Register {
        username: "newcomer".into(),
        display_name: "Newcomer".into(),
        password: "long enough".into(),
        invite_code: code.into(),
    });
    (driver, out)
}

#[test]
fn a_code_goes_with_the_registration_trimmed_and_blank_is_none() {
    let (_, out) = register("");
    let [Effect::Register(request)] = &out.effects[..] else {
        panic!("{:?}", out.effects);
    };
    assert!(request.invite_code.is_none());
    let (_, out) = register("   ");
    assert!(matches!(&out.effects[..], [Effect::Register(r)] if r.invite_code.is_none()));
    let (_, out) = register(&format!("  {CODE}\n"));
    let [Effect::Register(request)] = &out.effects[..] else {
        panic!("{:?}", out.effects);
    };
    assert_eq!(request.invite_code.as_ref().unwrap().expose(), CODE);
    // The request holds the code, but nothing prints it.
    assert!(!format!("{request:?}").contains(CODE));
}

#[test]
fn a_code_that_cannot_be_right_is_refused_without_a_request_and_not_repeated() {
    for bad in [
        "short",
        "has space in it but is long enough",
        "ünïcode-code-with-length-over-20",
        &"a".repeat(65),
    ] {
        let (driver, out) = register(bad);
        assert!(out.effects.is_empty(), "{bad:?}");
        let SessionPhase::SignedOut {
            last_error: Some(message),
        } = driver.core.phase()
        else {
            panic!("{:?}", driver.core.phase());
        };
        assert!(message.contains("invite code"));
        assert!(!message.contains(bad), "the code is never echoed");
    }
}

#[test]
fn refusals_to_register_have_their_own_wording() {
    let (mut driver, _) = register(CODE);
    driver.send(Input::AuthFailed(rejected(
        ErrorCode::InvitationInvalid,
        "server words",
    )));
    let SessionPhase::SignedOut {
        last_error: Some(message),
    } = driver.core.phase().clone()
    else {
        panic!();
    };
    assert!(message.contains("invite code"), "{message}");
    assert!(!message.contains(CODE));
    assert!(!message.contains("server words"));

    let (mut driver, _) = register("");
    driver.send(Input::AuthFailed(rejected(
        ErrorCode::Forbidden,
        "invite only",
    )));
    let SessionPhase::SignedOut {
        last_error: Some(message),
    } = driver.core.phase().clone()
    else {
        panic!();
    };
    assert!(message.contains("invite-only"), "{message}");

    // Anything else is the server's own words, and a sign-in is not worded as a registration.
    let (mut driver, _) = register("");
    driver.send(Input::AuthFailed(rejected(
        ErrorCode::UsernameTaken,
        "taken!",
    )));
    assert!(matches!(
        driver.core.phase(),
        SessionPhase::SignedOut { last_error: Some(m) } if m == "taken!"
    ));
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState::default()));
    driver.send(Input::SignIn {
        username: "newcomer".into(),
        password: "long enough".into(),
    });
    driver.send(Input::AuthFailed(rejected(ErrorCode::Forbidden, "words")));
    assert!(matches!(
        driver.core.phase(),
        SessionPhase::SignedOut { last_error: Some(m) } if m == "words"
    ));
}

#[test]
fn the_wording_of_a_refused_registration_does_not_stick_to_a_later_sign_in() {
    let (mut driver, _) = register("");
    driver.send(Input::AuthFailed(rejected(
        ErrorCode::Forbidden,
        "invite only",
    )));
    driver.send(Input::SignIn {
        username: "newcomer".into(),
        password: "long enough".into(),
    });
    driver.send(Input::AuthFailed(rejected(ErrorCode::Forbidden, "plain")));
    assert!(matches!(
        driver.core.phase(),
        SessionPhase::SignedOut { last_error: Some(m) } if m == "plain"
    ));
}

// ----- Losing access -----

fn lose_access(fx: &mut Fx) -> Output {
    let out = fx.driver.send(Input::Frame(ServerFrame::Error {
        code: ErrorCode::Forbidden,
        message: "you are no longer a member of that workspace".into(),
    }));
    assert!(
        out.effects.contains(&Effect::LoadBootstrap),
        "the bootstrap says whether access is gone: {:?}",
        out.effects
    );
    fx.driver.send(Input::BootstrapLoaded(Box::new(
        lazy_protocol::BootstrapResponse {
            protocol_version: lazy_protocol::PROTOCOL_VERSION,
            user: fx.world.me.clone(),
            workspaces: Vec::new(),
        },
    )))
}

#[test]
fn losing_access_empties_the_screen_names_the_workspace_and_stops_reading_it() {
    let mut fx = setup_as(WorkspaceRole::Member);
    let workspace = fx.world.workspace.id;
    assert_eq!(fx.driver.core.access_lost(), None);
    let out = lose_access(&mut fx);
    assert_eq!(fx.driver.core.phase(), &SessionPhase::Ready);
    assert_eq!(fx.driver.core.access_lost(), Some("Lazy"));
    assert!(fx.driver.core.primary_workspace().is_none());
    assert!(fx.driver.core.conversations().is_empty());
    assert_eq!(fx.driver.core.selected_conversation(), None);
    assert_eq!(fx.driver.core.my_role(), None);
    assert!(!fx.driver.core.can_manage_workspace());
    assert!(out.changes.contains(&Change::WorkspaceChanged));
    assert!(out.effects.contains(&Effect::Unsubscribe { workspace }));
    assert_eq!(fx.driver.core.notice(), None, "no raw error text");
    assert!(
        !out.effects
            .iter()
            .any(|e| matches!(e, Effect::PersistCache(_))),
        "nothing of the workspace is written back"
    );
    // What was cached is gone.
    assert!(
        fx.driver
            .effects
            .iter()
            .any(|e| matches!(e, Effect::DiscardCache))
    );
}

#[test]
fn an_inbox_read_answered_after_access_was_lost_does_not_leave_the_inbox_marking() {
    let mut fx = setup_as(WorkspaceRole::Member);
    let out = fx.driver.send(Input::OpenInbox);
    let generation = out
        .effects
        .iter()
        .find_map(|e| match e {
            Effect::LoadInbox { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("the inbox was asked for");
    fx.driver.send(Input::InboxLoaded {
        generation,
        result: Ok(lazy_protocol::InboxPage {
            entries: Vec::new(),
            has_more: false,
        }),
    });
    fx.driver.send(Input::MarkInboxRead);
    assert!(fx.driver.core.inbox().unwrap().marking_read);
    lose_access(&mut fx);
    // Everything of the workspace goes with it, the inbox among it; the answer that arrives
    // afterwards finds nothing to change.
    assert!(fx.driver.core.inbox().is_none());
    fx.driver.send(Input::InboxReadResult {
        result: Ok(lazy_protocol::InboxReadResponse { updated: 0 }),
    });
    assert!(fx.driver.core.inbox().is_none());
}

#[test]
fn regaining_access_clears_it_and_subscribes_again() {
    let mut fx = setup_as(WorkspaceRole::Member);
    lose_access(&mut fx);
    assert!(fx.driver.core.access_lost().is_some());
    // A later sign-in or bootstrap lists the workspace again.
    fx.driver.send(Input::RetryLoad);
    let out = fx
        .driver
        .send(Input::BootstrapLoaded(Box::new(fx.world.bootstrap())));
    assert_eq!(fx.driver.core.access_lost(), None);
    assert!(out.changes.contains(&Change::WorkspaceChanged));
    assert!(out.effects.iter().any(
        |e| matches!(e, Effect::Subscribe { workspace, .. } if *workspace == fx.world.workspace.id)
    ));
    assert_eq!(fx.driver.core.my_role(), Some(WorkspaceRole::Member));
    assert!(fx.driver.core.selected_conversation().is_some());
}

#[test]
fn an_account_that_never_had_a_workspace_has_not_lost_access() {
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        ..Default::default()
    }));
    let world = World::new();
    let out = driver.send(Input::BootstrapLoaded(Box::new(
        lazy_protocol::BootstrapResponse {
            protocol_version: lazy_protocol::PROTOCOL_VERSION,
            user: world.me.clone(),
            workspaces: Vec::new(),
        },
    )));
    assert_eq!(driver.core.phase(), &SessionPhase::Ready);
    assert_eq!(driver.core.access_lost(), None);
    assert!(!out.changes.contains(&Change::WorkspaceChanged));
}

#[test]
fn access_lost_while_the_app_was_closed_is_found_from_the_cache() {
    let world = World::new();
    let mut first = Driver::ready(&world);
    first.load_history(world.channel.id, vec![], false);
    let cache = first.last_cache().expect("saved").clone();

    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        cache: Some(cache),
        ..Default::default()
    }));
    assert!(driver.core.primary_workspace().is_some(), "restored");
    let out = driver.send(Input::BootstrapLoaded(Box::new(
        lazy_protocol::BootstrapResponse {
            protocol_version: lazy_protocol::PROTOCOL_VERSION,
            user: world.me.clone(),
            workspaces: Vec::new(),
        },
    )));
    assert_eq!(driver.core.access_lost(), Some("Lazy"));
    assert!(out.effects.contains(&Effect::DiscardCache));
    assert!(out.effects.contains(&Effect::Unsubscribe {
        workspace: world.workspace.id
    }));
    assert!(driver.core.conversations().is_empty());
}

#[test]
fn another_error_frame_is_still_just_a_notice() {
    let mut fx = setup_as(WorkspaceRole::Member);
    let out = fx.driver.send(Input::Frame(ServerFrame::Error {
        code: ErrorCode::InvalidRequest,
        message: "bad frame".into(),
    }));
    assert!(!out.effects.contains(&Effect::LoadBootstrap));
    assert_eq!(fx.driver.core.notice(), Some("bad frame"));
    assert_eq!(fx.driver.core.access_lost(), None);
}

#[test]
fn a_forbidden_frame_while_access_remains_costs_one_bootstrap_and_nothing_else() {
    // The person was deactivated and reactivated before the bootstrap came back.
    let mut fx = setup_as(WorkspaceRole::Member);
    let out = fx.driver.send(Input::Frame(ServerFrame::Error {
        code: ErrorCode::Forbidden,
        message: "gone".into(),
    }));
    assert!(out.effects.contains(&Effect::LoadBootstrap));
    fx.driver
        .send(Input::BootstrapLoaded(Box::new(fx.world.bootstrap())));
    assert_eq!(fx.driver.core.access_lost(), None);
    assert!(fx.driver.core.primary_workspace().is_some());
    assert_eq!(fx.driver.core.phase(), &SessionPhase::Ready);
}

#[test]
fn sign_in_after_losing_access_to_another_account_forgets_the_lost_workspace() {
    let mut fx = setup_as(WorkspaceRole::Member);
    lose_access(&mut fx);
    assert!(fx.driver.core.access_lost().is_some());
    fx.driver.send(Input::SignOut);
    assert_eq!(fx.driver.core.access_lost(), None);
}

#[test]
fn the_unused_helpers_are_used() {
    // Keeps `seq` and `UserId` imports honest for the helpers shared with the other suites.
    assert_eq!(seq(1).get(), 1);
    let _ = UserId::generate();
}
