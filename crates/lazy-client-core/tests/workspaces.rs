//! Several workspaces, creating and joining them, the person's profile and avatar, and the
//! appearance of the device (slice 3G, D23).

mod common;

use common::{Driver, World, channel, people, read_state, seq, user};
use lazy_client_core::{
    ActionKind, ActionOutcome, ActionState, Appearance, Change, ConnectionState, Effect, FileBytes,
    FileSource, ImageData, ImageState, Input, MotionChoice, Output, RequestError, SessionPhase,
    StartupState, ThemeChoice, TransferError, ViewState,
};
use lazy_domain::notification::NotifyLevel;
use lazy_domain::workspace::MAX_AVATAR_UPLOAD_BYTES;
use lazy_domain::{
    ConversationId, DisplayName, EventSeq, MessageId, Timestamp, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    ApiError, BootstrapResponse, Conversation, ConversationUserState, ErrorCode, Event,
    EventEnvelope, InviteCode, PROTOCOL_VERSION, ServerFrame, User, Workspace, WorkspaceBootstrap,
};

// ----- Fixture -----

const CODE: &str = "Zx9-secret_code_value_0123456789";

/// One workspace of the fixture.
struct Ws {
    workspace: Workspace,
    other: User,
    general: Conversation,
    direct: Conversation,
    role: WorkspaceRole,
    head: i64,
    direct_level: NotifyLevel,
    read: Vec<ConversationUserState>,
}

impl Ws {
    fn new(name: &str, me: &User, other_name: &str) -> Self {
        let workspace = Workspace {
            id: WorkspaceId::generate(),
            name: name.into(),
            settings: Default::default(),
        };
        let other = user(other_name);
        let mut general = channel(workspace.id, "general");
        general.is_default = true;
        let direct = people(workspace.id, &[me, &other]);
        Self {
            workspace,
            other,
            general,
            direct,
            role: WorkspaceRole::Member,
            head: 10,
            direct_level: NotifyLevel::All,
            read: Vec::new(),
        }
    }

    fn bootstrap(&self, me: &User) -> WorkspaceBootstrap {
        let settings = lazy_domain::notification::NotificationSettings {
            direct_level: self.direct_level,
            ..Default::default()
        };
        WorkspaceBootstrap {
            workspace: self.workspace.clone(),
            role: self.role,
            conversations: vec![self.general.clone(), self.direct.clone()],
            user_state: self.read.clone(),
            members: vec![me.clone(), self.other.clone()],
            groups: Vec::new(),
            custom_emoji: Vec::new(),
            notification_settings: settings,
            conversation_notification_prefs: Vec::new(),
            threads: Vec::new(),
            saved: Vec::new(),
            head: seq(self.head),
        }
    }

    fn event(&self, at: i64, event: Event) -> Input {
        Input::Frame(ServerFrame::Event(EventEnvelope {
            workspace_id: self.workspace.id,
            seq: seq(at),
            occurred_at: Timestamp::now(),
            event,
        }))
    }

    /// A direct message from the other person, as an event.
    fn direct_message(&self, at: i64, text: &str) -> (Input, MessageId) {
        let message = World::new().message(self.direct.id, &self.other, at, text);
        let id = message.id;
        let input = self.event(
            at,
            Event::MessageCreated {
                message,
                operation_id: None,
            },
        );
        (input, id)
    }
}

struct Fx {
    me: User,
    a: Ws,
    b: Ws,
    driver: Driver,
}

fn start_with(configure: impl FnOnce(&mut Ws, &mut Ws)) -> Fx {
    start_from(
        StartupState {
            session_restored: true,
            ..Default::default()
        },
        configure,
    )
}

fn start_from(state: StartupState, configure: impl FnOnce(&mut Ws, &mut Ws)) -> Fx {
    let me = user("jules");
    let mut a = Ws::new("Alpha Works", &me, "zoe");
    let mut b = Ws::new("Beta", &me, "bob");
    configure(&mut a, &mut b);
    let mut driver = Driver::new();
    driver.send(Input::Started(state));
    let response = BootstrapResponse {
        protocol_version: PROTOCOL_VERSION,
        user: me.clone(),
        workspaces: vec![a.bootstrap(&me), b.bootstrap(&me)],
    };
    driver.send(Input::BootstrapLoaded(Box::new(response)));
    assert_eq!(driver.core.phase(), &SessionPhase::Ready);
    Fx { me, a, b, driver }
}

fn start() -> Fx {
    start_with(|_, _| {})
}

impl Fx {
    fn switch(&mut self, workspace: WorkspaceId) -> Output {
        self.driver.send(Input::SwitchWorkspace(workspace))
    }

    fn active(&self) -> Option<WorkspaceId> {
        self.driver.core.active_workspace()
    }

    fn rail(&self) -> Vec<(String, bool, u32, bool)> {
        self.driver
            .core
            .workspace_rail()
            .into_iter()
            .map(|item| (item.name, item.active, item.attention, item.unread))
            .collect()
    }
}

fn notifications(out: &Output) -> Vec<lazy_client_core::DesktopNotification> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Notify(notification) => Some(notification.clone()),
            _ => None,
        })
        .collect()
}

fn badges(out: &Output) -> Vec<u32> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::SetBadge(count) => Some(*count),
            _ => None,
        })
        .collect()
}

fn rejected(code: ErrorCode, message: &str) -> RequestError {
    RequestError::Rejected(ApiError::new(code, message))
}

// ----- The rail and switching -----

#[test]
fn the_rail_lists_every_workspace_with_the_first_active() {
    let fx = start();
    let rail = fx.driver.core.workspace_rail();
    assert_eq!(rail.len(), 2);
    assert_eq!(rail[0].id, fx.a.workspace.id);
    assert_eq!(rail[0].name, "Alpha Works");
    assert_eq!(rail[0].initials, "AW");
    assert_eq!(rail[1].initials, "BE");
    assert!(rail[0].active && !rail[1].active);
    assert_eq!(fx.active(), Some(fx.a.workspace.id));
    assert_eq!(rail[0].role, WorkspaceRole::Member);
    assert_eq!(rail[0].connection, ConnectionState::Connecting);
    // Every workspace has its subscription.
    let subscribed: Vec<WorkspaceId> = fx.driver.effects_matching(|e| match e {
        Effect::Subscribe { workspace, .. } => Some(*workspace),
        _ => None,
    });
    assert!(subscribed.contains(&fx.a.workspace.id) && subscribed.contains(&fx.b.workspace.id));
}

#[test]
fn the_workspace_last_active_is_restored_if_it_is_still_listed() {
    let me = user("jules");
    let b = Ws::new("Beta", &me, "bob");
    let _ = b;
    // The preference names the second workspace of the bootstrap, whose id the fixture only
    // knows afterwards; start once to learn it, then start again with it.
    let first = start();
    let wanted = first.b.workspace.id;
    let mut state = StartupState {
        session_restored: true,
        active_workspace: Some(wanted),
        ..Default::default()
    };
    let me = first.me.clone();
    let mut driver = Driver::new();
    driver.send(Input::Started(state.clone()));
    let response = BootstrapResponse {
        protocol_version: PROTOCOL_VERSION,
        user: me.clone(),
        workspaces: vec![first.a.bootstrap(&me), first.b.bootstrap(&me)],
    };
    driver.send(Input::BootstrapLoaded(Box::new(response)));
    assert_eq!(driver.core.active_workspace(), Some(wanted));
    assert_eq!(
        driver.core.primary_workspace().unwrap().workspace.id,
        wanted
    );
    assert!(
        driver
            .core
            .conversations()
            .iter()
            .all(|c| c.workspace_id == wanted)
    );

    // A workspace that is gone is not a reason to show nothing.
    state.active_workspace = Some(WorkspaceId::generate());
    let mut driver = Driver::new();
    driver.send(Input::Started(state));
    let response = BootstrapResponse {
        protocol_version: PROTOCOL_VERSION,
        user: me.clone(),
        workspaces: vec![first.a.bootstrap(&me)],
    };
    driver.send(Input::BootstrapLoaded(Box::new(response)));
    assert_eq!(driver.core.active_workspace(), Some(first.a.workspace.id));
}

#[test]
fn switching_shows_the_other_workspace_and_keeps_the_first_for_later() {
    let mut fx = start();
    let (a, b) = (fx.a.workspace.id, fx.b.workspace.id);
    let a_general = fx.a.general.id;
    assert_eq!(fx.driver.core.selected_conversation(), Some(a_general));
    let out = fx.switch(b);
    assert!(out.changes.contains(&Change::ActiveWorkspaceChanged));
    assert!(out.changes.contains(&Change::RailChanged));
    assert!(
        out.effects
            .contains(&Effect::PersistActiveWorkspace(Some(b)))
    );
    assert_eq!(fx.active(), Some(b));
    assert_eq!(
        fx.driver.core.primary_workspace().unwrap().workspace.name,
        "Beta"
    );
    // The screen is Beta's, whole.
    let shown: Vec<ConversationId> = fx
        .driver
        .core
        .conversations()
        .iter()
        .map(|c| c.id)
        .collect();
    assert!(shown.contains(&fx.b.general.id) && !shown.contains(&a_general));
    assert!(
        fx.driver
            .core
            .members()
            .iter()
            .any(|m| m.id == fx.b.other.id)
    );
    assert!(
        !fx.driver
            .core
            .members()
            .iter()
            .any(|m| m.id == fx.a.other.id)
    );
    assert_eq!(
        fx.driver.core.selected_conversation(),
        Some(fx.b.general.id)
    );
    assert_eq!(fx.driver.core.sidebar().channels, vec![fx.b.general.id]);
    // Its history is asked for; the other's conversation is still known by id.
    assert!(fx.driver.history_requests().contains(&fx.b.general.id));
    assert!(fx.driver.core.conversation(a_general).is_some());

    // Back: the conversation that was open there is open again.
    fx.driver.send(Input::SelectConversation(fx.a.direct.id));
    fx.switch(b);
    fx.switch(a);
    assert_eq!(fx.driver.core.selected_conversation(), Some(fx.a.direct.id));
    // Switching to where it is, or to a stranger, does nothing.
    let out = fx.switch(a);
    assert!(out.changes.is_empty());
    let out = fx.switch(WorkspaceId::generate());
    assert!(out.changes.is_empty());
    assert_eq!(fx.active(), Some(a));
}

#[test]
fn a_person_in_both_workspaces_is_deactivated_in_one_and_named_in_both() {
    let me = user("jules");
    let shared = user("sam");
    let mut a = Ws::new("Alpha", &me, "zoe");
    let mut b = Ws::new("Beta", &me, "bob");
    a.read = Vec::new();
    b.read = Vec::new();
    let mut shared_b = shared.clone();
    shared_b.deactivated = true;
    let mut ba = a.bootstrap(&me);
    ba.members.push(shared.clone());
    let mut bb = b.bootstrap(&me);
    bb.members.push(shared_b);
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        ..Default::default()
    }));
    driver.send(Input::BootstrapLoaded(Box::new(BootstrapResponse {
        protocol_version: PROTOCOL_VERSION,
        user: me,
        workspaces: vec![ba, bb],
    })));
    assert!(!driver.core.member(shared.id).unwrap().deactivated);
    assert!(driver.core.members().iter().any(|m| m.id == shared.id));
    driver.send(Input::SwitchWorkspace(b.workspace.id));
    assert!(
        driver.core.member(shared.id).unwrap().deactivated,
        "deactivated there"
    );
    assert!(!driver.core.members().iter().any(|m| m.id == shared.id));
    assert_eq!(driver.core.display_name(shared.id), "SAM");
}

#[test]
fn switching_closes_the_views_of_the_workspace_left() {
    let mut fx = start();
    fx.driver.send(Input::OpenInbox);
    fx.driver.send(Input::OpenSearch);
    fx.driver.send(Input::OpenSaved);
    fx.driver.send(Input::OpenPeople);
    assert!(fx.driver.core.inbox().is_some() && fx.driver.core.search().is_some());
    fx.switch(fx.b.workspace.id);
    assert!(fx.driver.core.inbox().is_none());
    assert!(fx.driver.core.search().is_none());
    assert!(!fx.driver.core.saved().open);
    assert!(!fx.driver.core.people().open);
    // And they do not come back with the workspace.
    fx.switch(fx.a.workspace.id);
    assert!(fx.driver.core.inbox().is_none());
    assert!(!fx.driver.core.people().open);
}

#[test]
fn workspace_settings_notification_settings_and_roles_are_each_workspaces_own() {
    let mut fx = start_with(|a, b| {
        a.role = WorkspaceRole::Owner;
        b.role = WorkspaceRole::Member;
        b.direct_level = NotifyLevel::Nothing;
    });
    assert!(fx.driver.core.can_manage_workspace());
    assert_eq!(
        fx.driver.core.notification_settings().direct_level,
        NotifyLevel::All
    );
    fx.switch(fx.b.workspace.id);
    assert!(!fx.driver.core.can_manage_workspace());
    assert_eq!(fx.driver.core.my_role(), Some(WorkspaceRole::Member));
    assert_eq!(
        fx.driver.core.notification_settings().direct_level,
        NotifyLevel::Nothing
    );
    let rail = fx.driver.core.workspace_rail();
    assert_eq!(rail[0].role, WorkspaceRole::Owner);
}

// ----- What happens in the background -----

#[test]
fn a_message_in_the_background_counts_for_its_workspace_and_for_the_total() {
    let mut fx = start();
    let (input, _) = fx.b.direct_message(11, "hello from beta");
    let out = fx.driver.send(input);
    assert_eq!(
        fx.driver.core.attention_count(),
        0,
        "the active workspace's own"
    );
    assert_eq!(fx.driver.core.attention_total(), 1);
    assert_eq!(
        fx.rail(),
        vec![
            ("Alpha Works".to_owned(), true, 0, false),
            ("Beta".to_owned(), false, 1, false),
        ]
    );
    assert!(out.changes.contains(&Change::RailChanged));
    assert_eq!(badges(&out), vec![1], "the badge is the total");
    // Nothing of it is on Alpha's screen.
    assert!(
        fx.driver
            .core
            .conversations()
            .iter()
            .all(|c| c.workspace_id == fx.a.workspace.id)
    );
    assert_eq!(fx.driver.core.total_unread(), 0);
    // Switching shows the same number as the active one.
    fx.switch(fx.b.workspace.id);
    assert_eq!(fx.driver.core.attention_count(), 1);
    assert_eq!(fx.driver.core.attention_total(), 1);
    assert_eq!(fx.rail()[0], ("Alpha Works".to_owned(), false, 0, false));
}

#[test]
fn a_channel_message_that_names_nobody_is_a_dot_not_a_number() {
    let mut fx = start();
    let message = World::new().message(fx.b.general.id, &fx.b.other, 11, "no mention");
    fx.driver.send(fx.b.event(
        11,
        Event::MessageCreated {
            message,
            operation_id: None,
        },
    ));
    assert_eq!(fx.rail()[1], ("Beta".to_owned(), false, 0, true));
    assert_eq!(fx.driver.core.attention_total(), 0);
}

#[test]
fn a_background_notification_names_its_workspace_and_follows_its_settings() {
    let mut fx = start_with(|_, b| b.direct_level = NotifyLevel::All);
    let (input, id) = fx.b.direct_message(11, "ping");
    // It is news once the connection has caught up.
    fx.driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: fx.b.workspace.id,
        seq: seq(10),
    }));
    let out = fx.driver.send(input);
    let shown = notifications(&out);
    assert_eq!(shown.len(), 1, "{:?}", out.effects);
    assert_eq!(shown[0].message, id);
    assert!(shown[0].title.starts_with("Beta · "), "{}", shown[0].title);

    // In the active workspace the title is as it was.
    let (input, _) = fx.a.direct_message(11, "ping");
    fx.driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: fx.a.workspace.id,
        seq: seq(10),
    }));
    let out = fx.driver.send(input);
    let shown = notifications(&out);
    assert_eq!(shown.len(), 1);
    assert!(!shown[0].title.contains('·'), "{}", shown[0].title);

    // A workspace whose settings say nothing sends nothing, whatever the other's say.
    let mut fx = start_with(|_, b| b.direct_level = NotifyLevel::Nothing);
    fx.driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: fx.b.workspace.id,
        seq: seq(10),
    }));
    let (input, _) = fx.b.direct_message(11, "ping");
    let out = fx.driver.send(input);
    assert!(notifications(&out).is_empty());
}

#[test]
fn a_notification_is_never_titled_with_a_workspace_when_there_is_only_one() {
    let me = user("jules");
    let a = Ws::new("Alpha", &me, "zoe");
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        ..Default::default()
    }));
    driver.send(Input::BootstrapLoaded(Box::new(BootstrapResponse {
        protocol_version: PROTOCOL_VERSION,
        user: me,
        workspaces: vec![a.bootstrap(&user("jules"))],
    })));
    driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: a.workspace.id,
        seq: seq(10),
    }));
    let (input, _) = a.direct_message(11, "ping");
    let out = driver.send(input);
    assert!(notifications(&out).iter().all(|n| !n.title.contains('·')));
}

#[test]
fn clicking_a_background_notification_switches_to_its_workspace_and_opens_the_message() {
    let mut fx = start();
    fx.driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: fx.b.workspace.id,
        seq: seq(10),
    }));
    let (input, id) = fx.b.direct_message(11, "ping");
    fx.driver.send(input);
    let out = fx.driver.send(Input::NotificationActivated {
        conversation: fx.b.direct.id,
        message: id,
    });
    assert!(out.changes.contains(&Change::ActiveWorkspaceChanged));
    assert_eq!(fx.active(), Some(fx.b.workspace.id));
    assert_eq!(fx.driver.core.selected_conversation(), Some(fx.b.direct.id));
}

#[test]
fn events_of_a_workspace_in_the_background_do_not_touch_the_active_one() {
    let mut fx = start();
    let joined = user("newcomer");
    fx.driver.send(fx.b.event(
        11,
        Event::MemberJoined {
            user: joined.clone(),
        },
    ));
    assert!(!fx.driver.core.members().iter().any(|m| m.id == joined.id));
    assert!(
        fx.driver.core.member(joined.id).is_none(),
        "Beta's person, not Alpha's"
    );
    assert_eq!(fx.driver.core.display_name(joined.id), "NEWCOMER");
    fx.switch(fx.b.workspace.id);
    assert!(fx.driver.core.members().iter().any(|m| m.id == joined.id));
    // A workspace's name changes in the background and the rail follows.
    let mut renamed = fx.a.workspace.clone();
    renamed.name = "Alpha Two".into();
    let out = fx
        .driver
        .send(fx.a.event(11, Event::WorkspaceUpdated { workspace: renamed }));
    assert!(out.changes.contains(&Change::RailChanged));
    assert_eq!(fx.rail()[0].0, "Alpha Two");
    // And a deactivation in the background is that workspace's.
    fx.driver.send(fx.a.event(
        12,
        Event::MemberDeactivated {
            user_id: fx.a.other.id,
            deactivated: true,
        },
    ));
    assert!(
        !fx.driver
            .core
            .members()
            .iter()
            .any(|m| m.id == fx.a.other.id),
        "Alpha's people are not Beta's"
    );
    fx.switch(fx.a.workspace.id);
    assert!(fx.driver.core.member(fx.a.other.id).unwrap().deactivated);
}

#[test]
fn reading_in_the_active_workspace_moves_only_its_own_count() {
    let mut fx = start_with(|a, b| {
        a.read = vec![read_state(a.direct.id, 5, 2)];
        b.read = vec![read_state(b.direct.id, 5, 3)];
    });
    assert_eq!(fx.rail()[0].2, 2);
    assert_eq!(fx.rail()[1].2, 3);
    assert_eq!(fx.driver.core.attention_total(), 5);
    // Muting a conversation of the background workspace takes it out of its count.
    fx.driver.send(Input::SetConversationNotificationPrefs {
        conversation: fx.b.direct.id,
        level: None,
        muted: true,
    });
    assert_eq!(fx.rail()[1].2, 0);
    assert_eq!(fx.driver.core.attention_total(), 2);
}

#[test]
fn losing_one_workspace_removes_it_from_the_rail_and_another_takes_its_place() {
    let mut fx = start();
    fx.switch(fx.b.workspace.id);
    let b_direct = fx.b.direct.id;
    // The next bootstrap lists only Alpha.
    let me = fx.me.clone();
    let out = fx
        .driver
        .send(Input::BootstrapLoaded(Box::new(BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: me.clone(),
            workspaces: vec![fx.a.bootstrap(&me)],
        })));
    assert_eq!(fx.active(), Some(fx.a.workspace.id));
    assert_eq!(fx.driver.core.workspace_rail().len(), 1);
    assert!(out.effects.contains(&Effect::Unsubscribe {
        workspace: fx.b.workspace.id
    }));
    assert!(fx.driver.core.notice().unwrap_or_default().contains("Beta"));
    assert!(
        fx.driver.core.conversation(b_direct).is_none(),
        "nothing of it remains"
    );
    assert_eq!(
        fx.driver.core.access_lost(),
        None,
        "another workspace remains"
    );
    assert!(fx.driver.core.selected_conversation().is_some());
}

// ----- Creating a workspace -----

fn created_workspace(name: &str, me: &User) -> (WorkspaceBootstrap, Conversation) {
    let ws = Ws::new(name, me, "xavier");
    let mut bootstrap = ws.bootstrap(me);
    bootstrap.role = WorkspaceRole::Owner;
    bootstrap.members = vec![me.clone()];
    bootstrap.conversations = vec![ws.general.clone()];
    bootstrap.head = seq(3);
    (bootstrap, ws.general)
}

#[test]
fn a_workspace_is_created_with_an_id_chosen_once_and_becomes_active() {
    let mut fx = start();
    let out = fx.driver.send(Input::CreateWorkspace {
        name: "  Gamma  ".into(),
    });
    let sent: Vec<_> = out
        .effects
        .iter()
        .filter_map(|e| match e {
            Effect::CreateWorkspace { workspace, request } => Some((*workspace, request.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(sent.len(), 1, "{:?}", out.effects);
    let (workspace, request) = sent.into_iter().next().unwrap();
    assert_eq!(request.name.as_str(), "Gamma");
    assert_eq!(
        fx.driver.core.action(ActionKind::CreateWorkspace),
        &ActionState::InFlight
    );
    // A second press while it is out is ignored.
    let out = fx.driver.send(Input::CreateWorkspace {
        name: "Other".into(),
    });
    assert!(
        !out.effects
            .iter()
            .any(|e| matches!(e, Effect::CreateWorkspace { .. }))
    );

    let (mut bootstrap, general) = created_workspace("Gamma", &fx.me);
    bootstrap.workspace.id = workspace;
    let mut general = general;
    general.workspace_id = workspace;
    bootstrap.conversations = vec![general.clone()];
    let out = fx.driver.send(Input::ActionResult {
        kind: ActionKind::CreateWorkspace,
        result: Ok(ActionOutcome::Joined(Box::new(bootstrap))),
    });
    assert_eq!(fx.active(), Some(workspace));
    assert_eq!(fx.driver.core.workspace_rail().len(), 3);
    assert_eq!(fx.driver.core.my_role(), Some(WorkspaceRole::Owner));
    assert!(out.effects.contains(&Effect::Subscribe {
        workspace,
        after: seq(3)
    }));
    assert!(
        out.effects
            .contains(&Effect::PersistActiveWorkspace(Some(workspace)))
    );
    assert_eq!(fx.driver.core.selected_conversation(), Some(general.id));
    assert_eq!(
        fx.driver.core.action(ActionKind::CreateWorkspace),
        &ActionState::Idle
    );
    // The others are as they were.
    assert_eq!(fx.rail()[0].0, "Alpha Works");
}

#[test]
fn a_name_that_cannot_be_one_asks_for_nothing() {
    let mut fx = start();
    for name in ["", "   ", &"x".repeat(65)] {
        let out = fx.driver.send(Input::CreateWorkspace { name: name.into() });
        assert!(
            !out.effects
                .iter()
                .any(|e| matches!(e, Effect::CreateWorkspace { .. }))
        );
        assert!(matches!(
            fx.driver.core.action(ActionKind::CreateWorkspace),
            ActionState::Failed(_)
        ));
    }
}

#[test]
fn an_unknown_outcome_repeats_the_same_request_three_times_and_then_stops() {
    let mut fx = start();
    let out = fx.driver.send(Input::CreateWorkspace {
        name: "Gamma".into(),
    });
    let first = out
        .effects
        .iter()
        .find_map(|e| match e {
            Effect::CreateWorkspace { workspace, .. } => Some(*workspace),
            _ => None,
        })
        .unwrap();
    let mut sent = 1;
    for attempt in 0..3 {
        let out = fx.driver.send(Input::ActionResult {
            kind: ActionKind::CreateWorkspace,
            result: Err(RequestError::Unknown("lost".into())),
        });
        let again: Vec<WorkspaceId> = out
            .effects
            .iter()
            .filter_map(|e| match e {
                Effect::CreateWorkspace { workspace, .. } => Some(*workspace),
                _ => None,
            })
            .collect();
        if attempt < 2 {
            assert_eq!(again, vec![first], "the same id");
            sent += 1;
            assert_eq!(
                fx.driver.core.action(ActionKind::CreateWorkspace),
                &ActionState::InFlight
            );
        } else {
            assert!(again.is_empty(), "gave up");
            assert!(matches!(
                fx.driver.core.action(ActionKind::CreateWorkspace),
                ActionState::Failed(_)
            ));
        }
    }
    assert_eq!(sent, 3);
}

#[test]
fn refusals_to_create_a_workspace_say_why() {
    for (code, needle) in [
        (ErrorCode::LimitReached, "as many workspaces"),
        (ErrorCode::Forbidden, "doesn't allow"),
    ] {
        let mut fx = start();
        fx.driver.send(Input::CreateWorkspace {
            name: "Gamma".into(),
        });
        fx.driver.send(Input::ActionResult {
            kind: ActionKind::CreateWorkspace,
            result: Err(rejected(code, "")),
        });
        let ActionState::Failed(text) = fx.driver.core.action(ActionKind::CreateWorkspace).clone()
        else {
            panic!("expected a failure");
        };
        assert!(text.contains(needle), "{text}");
        assert_eq!(fx.driver.core.workspace_rail().len(), 2);
    }
}

// ----- Joining with a code -----

#[test]
fn a_code_is_checked_here_sent_once_and_never_shown() {
    let mut fx = start();
    for bad in ["", "short", "has spaces in the code and is long enough"] {
        let out = fx.driver.send(Input::AcceptInvitation { code: bad.into() });
        assert!(
            !out.effects
                .iter()
                .any(|e| matches!(e, Effect::AcceptInvitation { .. }))
        );
        let ActionState::Failed(text) = fx.driver.core.action(ActionKind::AcceptInvitation).clone()
        else {
            panic!("expected a failure");
        };
        assert!(!text.contains(bad) || bad.is_empty());
    }
    let out = fx.driver.send(Input::AcceptInvitation {
        code: format!("  {CODE} "),
    });
    let sent = out.effects.iter().find_map(|e| match e {
        Effect::AcceptInvitation { request } => Some(request.code.clone()),
        _ => None,
    });
    assert_eq!(sent, Some(InviteCode::new(CODE)));
    assert!(!format!("{out:?}").contains(CODE));
    assert!(!format!("{:?}", fx.driver.effects).contains(CODE));
}

#[test]
fn joining_adds_the_workspace_and_makes_it_active_and_joining_one_you_are_in_just_shows_it() {
    let mut fx = start();
    let me = fx.me.clone();
    let delta = Ws::new("Delta Force", &me, "dan");
    fx.driver
        .send(Input::AcceptInvitation { code: CODE.into() });
    let out = fx.driver.send(Input::ActionResult {
        kind: ActionKind::AcceptInvitation,
        result: Ok(ActionOutcome::Joined(Box::new(delta.bootstrap(&me)))),
    });
    assert_eq!(fx.active(), Some(delta.workspace.id));
    assert_eq!(fx.driver.core.workspace_rail().len(), 3);
    assert_eq!(fx.driver.core.workspace_rail()[2].initials, "DF");
    assert!(out.effects.iter().any(|e| matches!(
        e,
        Effect::Subscribe { workspace, after } if *workspace == delta.workspace.id && *after == seq(10)
    )));
    // Its content is there.
    assert!(
        fx.driver
            .core
            .members()
            .iter()
            .any(|m| m.id == delta.other.id)
    );

    // The code of a workspace already joined returns it; nothing is added.
    fx.driver
        .send(Input::AcceptInvitation { code: CODE.into() });
    fx.driver.send(Input::ActionResult {
        kind: ActionKind::AcceptInvitation,
        result: Ok(ActionOutcome::Joined(Box::new(fx.a.bootstrap(&me)))),
    });
    assert_eq!(fx.driver.core.workspace_rail().len(), 3);
    assert_eq!(fx.active(), Some(fx.a.workspace.id));
}

#[test]
fn a_refused_code_has_words_of_its_own() {
    let mut fx = start();
    fx.driver
        .send(Input::AcceptInvitation { code: CODE.into() });
    fx.driver.send(Input::ActionResult {
        kind: ActionKind::AcceptInvitation,
        result: Err(rejected(ErrorCode::InvitationInvalid, "server words")),
    });
    let ActionState::Failed(text) = fx.driver.core.action(ActionKind::AcceptInvitation).clone()
    else {
        panic!("expected a failure");
    };
    assert!(text.contains("invite code") && !text.contains("server words") && !text.contains(CODE));
    fx.driver
        .send(Input::AcceptInvitation { code: CODE.into() });
    fx.driver.send(Input::ActionResult {
        kind: ActionKind::AcceptInvitation,
        result: Err(RequestError::Unknown("lost".into())),
    });
    // An unknown outcome is not repeated: it may have counted a use.
    assert!(matches!(
        fx.driver.core.action(ActionKind::AcceptInvitation),
        ActionState::Failed(_)
    ));
}

// ----- The person's profile and avatar -----

#[test]
fn a_new_name_is_checked_sent_and_then_everywhere() {
    let mut fx = start();
    for bad in ["", "   "] {
        let out = fx.driver.send(Input::UpdateProfile {
            display_name: Some(bad.into()),
        });
        assert!(
            !out.effects
                .iter()
                .any(|e| matches!(e, Effect::UpdateProfile { .. }))
        );
    }
    let out = fx.driver.send(Input::UpdateProfile {
        display_name: Some("  Jules R  ".into()),
    });
    assert!(out.effects.iter().any(|e| matches!(
        e,
        Effect::UpdateProfile { request } if request.display_name.as_ref().map(|n| n.as_str()) == Some("Jules R")
    )));
    let mut renamed = fx.me.clone();
    renamed.display_name = DisplayName::parse("Jules R").unwrap();
    renamed.avatar_version = 2;
    fx.driver.send(Input::ActionResult {
        kind: ActionKind::UpdateProfile,
        result: Ok(ActionOutcome::Profile(renamed)),
    });
    assert_eq!(
        fx.driver.core.me().unwrap().display_name.as_str(),
        "Jules R"
    );
    assert_eq!(fx.driver.core.display_name(fx.me.id), "Jules R");
    fx.switch(fx.b.workspace.id);
    assert_eq!(
        fx.driver.core.display_name(fx.me.id),
        "Jules R",
        "the same person in both"
    );
    assert_eq!(fx.driver.core.member(fx.me.id).unwrap().avatar_version, 2);
}

#[test]
fn someone_else_changing_their_name_reaches_every_workspace_they_share_with_the_person() {
    let me = user("jules");
    let shared = user("sam");
    let a = Ws::new("Alpha", &me, "zoe");
    let b = Ws::new("Beta", &me, "bob");
    let mut ba = a.bootstrap(&me);
    ba.members.push(shared.clone());
    let mut bb = b.bootstrap(&me);
    bb.members.push(shared.clone());
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        ..Default::default()
    }));
    driver.send(Input::BootstrapLoaded(Box::new(BootstrapResponse {
        protocol_version: PROTOCOL_VERSION,
        user: me,
        workspaces: vec![ba, bb],
    })));
    // The event comes in Beta's log (background) only: Alpha's copy follows, being the same person.
    let out = driver.send(b.event(
        11,
        Event::UserProfileChanged {
            user_id: shared.id,
            display_name: DisplayName::parse("Sam Q").unwrap(),
            avatar_version: 3,
        },
    ));
    assert!(out.changes.contains(&Change::DirectoryChanged));
    assert_eq!(driver.core.display_name(shared.id), "Sam Q");
    assert_eq!(driver.core.member(shared.id).unwrap().avatar_version, 3);
    driver.send(Input::SwitchWorkspace(b.workspace.id));
    assert_eq!(driver.core.display_name(shared.id), "Sam Q");
}

fn png(n: u8) -> ImageData {
    ImageData::new(vec![n; 64])
}

fn fetches(out: &Output) -> Vec<(lazy_domain::UserId, u32)> {
    out.effects
        .iter()
        .filter_map(|e| match e {
            Effect::FetchAvatar { user, version } => Some((*user, *version)),
            _ => None,
        })
        .collect()
}

fn with_avatar(fx: &mut Fx, version: u32) {
    let other = fx.a.other.clone();
    fx.driver.send(fx.a.event(
        11,
        Event::UserProfileChanged {
            user_id: other.id,
            display_name: other.display_name.clone(),
            avatar_version: version,
        },
    ));
}

#[test]
fn an_avatar_is_asked_for_at_its_version_and_shown_when_it_arrives() {
    let mut fx = start();
    let other = fx.a.other.id;
    // No picture: nothing is fetched and nothing is held.
    let out = fx.driver.send(Input::LoadAvatar(other));
    assert!(fetches(&out).is_empty());
    assert!(fx.driver.core.avatar(other).is_none());

    with_avatar(&mut fx, 4);
    let out = fx.driver.send(Input::LoadAvatar(other));
    assert_eq!(fetches(&out), vec![(other, 4)]);
    assert_eq!(fx.driver.core.avatar(other), Some(&ImageState::Loading));
    // Asking again while it is on its way asks for nothing more.
    let out = fx.driver.send(Input::LoadAvatar(other));
    assert!(fetches(&out).is_empty());
    let out = fx.driver.send(Input::AvatarLoaded {
        user: other,
        version: 4,
        result: Ok(png(1)),
    });
    assert!(out.changes.contains(&Change::AvatarChanged { user: other }));
    assert!(matches!(
        fx.driver.core.avatar(other),
        Some(ImageState::Ready(_))
    ));
    let out = fx.driver.send(Input::LoadAvatar(other));
    assert!(fetches(&out).is_empty(), "held");
}

#[test]
fn a_new_version_drops_the_old_picture_and_fetches_the_new_one() {
    let mut fx = start();
    let other = fx.a.other.id;
    with_avatar(&mut fx, 1);
    fx.driver.send(Input::LoadAvatar(other));
    fx.driver.send(Input::AvatarLoaded {
        user: other,
        version: 1,
        result: Ok(png(1)),
    });
    fx.head_up();
    let out = fx.driver.send(fx.a.event(
        12,
        Event::UserProfileChanged {
            user_id: other,
            display_name: fx.a.other.display_name.clone(),
            avatar_version: 2,
        },
    ));
    assert!(
        out.effects
            .contains(&Effect::ForgetAvatars { users: vec![other] })
    );
    assert_eq!(fetches(&out), vec![(other, 2)]);
    assert_eq!(fx.driver.core.avatar(other), Some(&ImageState::Loading));
    // The old version's late answer is not taken for the new.
    fx.driver.send(Input::AvatarLoaded {
        user: other,
        version: 1,
        result: Ok(png(9)),
    });
    assert_eq!(fx.driver.core.avatar(other), Some(&ImageState::Loading));
    fx.driver.send(Input::AvatarLoaded {
        user: other,
        version: 2,
        result: Ok(png(2)),
    });
    assert!(matches!(
        fx.driver.core.avatar(other),
        Some(ImageState::Ready(_))
    ));
    // Removing the picture drops it and fetches nothing.
    let out = fx.driver.send(fx.a.event(
        13,
        Event::UserProfileChanged {
            user_id: other,
            display_name: fx.a.other.display_name.clone(),
            avatar_version: 0,
        },
    ));
    assert!(fetches(&out).is_empty());
    assert!(fx.driver.core.avatar(other).is_none());
}

impl Fx {
    /// Nothing: the fixture's events carry their own positions.
    fn head_up(&mut self) {}
}

#[test]
fn a_failed_fetch_is_failed_until_asked_for_again() {
    let mut fx = start();
    let other = fx.a.other.id;
    with_avatar(&mut fx, 1);
    fx.driver.send(Input::LoadAvatar(other));
    fx.driver.send(Input::AvatarLoaded {
        user: other,
        version: 1,
        result: Err(TransferError::Request(RequestError::Unknown("x".into()))),
    });
    assert_eq!(fx.driver.core.avatar(other), Some(&ImageState::Failed));
    let out = fx.driver.send(Input::LoadAvatar(other));
    assert_eq!(fetches(&out), vec![(other, 1)], "asking again tries again");
}

#[test]
fn the_pictures_held_are_bounded_and_the_least_recently_used_go_first() {
    let mut fx = start();
    let mut people = Vec::new();
    for n in 0..(lazy_client_core::MAX_HELD_AVATARS + 5) {
        let mut person = user(&format!("person{n}"));
        person.avatar_version = 1;
        people.push(person.id);
        fx.driver
            .send(fx.a.event(11 + n as i64, Event::MemberJoined { user: person }));
    }
    for id in &people {
        let out = fx.driver.send(Input::LoadAvatar(*id));
        for (user, version) in fetches(&out) {
            fx.driver.send(Input::AvatarLoaded {
                user,
                version,
                result: Ok(png(1)),
            });
        }
        // Whatever was queued behind it is fetched as the slots free up.
    }
    let held = people
        .iter()
        .filter(|id| matches!(fx.driver.core.avatar(**id), Some(ImageState::Ready(_))))
        .count();
    assert!(held <= lazy_client_core::MAX_HELD_AVATARS, "{held}");
    assert!(matches!(
        fx.driver.core.avatar(*people.last().unwrap()),
        Some(ImageState::Ready(_))
    ));
}

#[test]
fn a_picture_is_sent_to_the_runtime_unless_it_is_too_large_and_removal_needs_one() {
    let mut fx = start();
    let big = FileBytes::new(vec![0u8; MAX_AVATAR_UPLOAD_BYTES + 1]);
    let out = fx.driver.send(Input::SetAvatar {
        source: FileSource::Bytes(big),
    });
    assert!(
        !out.effects
            .iter()
            .any(|e| matches!(e, Effect::SetAvatar { .. }))
    );
    assert!(matches!(
        fx.driver.core.action(ActionKind::SetAvatar),
        ActionState::Failed(text) if text.contains("too large")
    ));
    let out = fx.driver.send(Input::SetAvatar {
        source: FileSource::Bytes(FileBytes::new(vec![1, 2, 3])),
    });
    assert!(
        out.effects
            .iter()
            .any(|e| matches!(e, Effect::SetAvatar { .. }))
    );
    let mut changed = fx.me.clone();
    changed.avatar_version = 1;
    fx.driver.send(Input::ActionResult {
        kind: ActionKind::SetAvatar,
        result: Ok(ActionOutcome::Profile(changed)),
    });
    assert_eq!(fx.driver.core.me().unwrap().avatar_version, 1);
    assert_eq!(
        fx.driver.core.action(ActionKind::SetAvatar),
        &ActionState::Idle
    );
    let out = fx.driver.send(Input::RemoveAvatar);
    assert!(out.effects.contains(&Effect::RemoveAvatar));
    // Without one, there is nothing to remove.
    let mut fresh = start();
    let out = fresh.driver.send(Input::RemoveAvatar);
    assert!(!out.effects.contains(&Effect::RemoveAvatar));
}

#[test]
fn a_refused_picture_says_why() {
    for (code, needle) in [
        (ErrorCode::UnsupportedMedia, "PNG, JPEG, WebP, or GIF"),
        (ErrorCode::PayloadTooLarge, "too large"),
    ] {
        let mut fx = start();
        fx.driver.send(Input::SetAvatar {
            source: FileSource::Bytes(FileBytes::new(vec![1, 2, 3])),
        });
        fx.driver.send(Input::ActionResult {
            kind: ActionKind::SetAvatar,
            result: Err(rejected(code, "")),
        });
        let ActionState::Failed(text) = fx.driver.core.action(ActionKind::SetAvatar).clone() else {
            panic!("expected a failure");
        };
        assert!(text.contains(needle), "{text}");
    }
}

// ----- Appearance -----

#[test]
fn the_appearance_applies_at_once_is_saved_and_comes_back_at_start() {
    let mut fx = start();
    assert_eq!(fx.driver.core.appearance(), Appearance::default());
    let dark = Appearance {
        theme: ThemeChoice::Dark,
        motion: MotionChoice::Reduce,
    };
    let out = fx.driver.send(Input::SetAppearance(dark));
    assert_eq!(fx.driver.core.appearance(), dark);
    assert!(out.changes.contains(&Change::AppearanceChanged));
    assert!(out.effects.contains(&Effect::PersistAppearance(dark)));
    let out = fx.driver.send(Input::SetAppearance(dark));
    assert!(out.is_empty(), "the same again changes nothing");

    let fresh = start_from(
        StartupState {
            session_restored: true,
            appearance: dark,
            ..Default::default()
        },
        |_, _| {},
    );
    assert_eq!(fresh.driver.core.appearance(), dark);
}

// ----- The cache -----

#[test]
fn a_cache_with_several_workspaces_restores_all_of_them_each_with_its_selection_and_drafts() {
    let mut fx = start();
    fx.driver.send(Input::SelectConversation(fx.a.direct.id));
    fx.driver.send(Input::SetDraft {
        conversation: fx.a.direct.id,
        text: "half a thought".into(),
    });
    fx.switch(fx.b.workspace.id);
    fx.driver.send(Input::SelectConversation(fx.b.direct.id));
    fx.driver.send(Input::SetDraft {
        conversation: fx.b.direct.id,
        text: "another".into(),
    });
    // Load history so the cache has timelines to keep.
    fx.driver.load_history(fx.b.direct.id, vec![], false);
    fx.switch(fx.a.workspace.id);
    fx.driver.load_history(fx.a.direct.id, vec![], false);
    let cache = fx.driver.last_cache().expect("a cache was saved").clone();
    assert_eq!(cache.workspaces.len(), 2);
    let view: ViewState = {
        // The view state the core asked to save (selections per workspace).
        fx.driver
            .effects_matching(|e| match e {
                Effect::PersistView(view) => Some(view.clone()),
                _ => None,
            })
            .pop()
            .expect("a view was saved")
    };
    let drafts = std::collections::HashMap::from([
        (fx.a.direct.id, "half a thought".to_owned()),
        (fx.b.direct.id, "another".to_owned()),
    ]);

    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        cache: Some(cache),
        view,
        drafts,
        active_workspace: Some(fx.b.workspace.id),
        ..Default::default()
    }));
    assert_eq!(driver.core.workspace_rail().len(), 2);
    assert_eq!(driver.core.active_workspace(), Some(fx.b.workspace.id));
    assert_eq!(driver.core.selected_conversation(), Some(fx.b.direct.id));
    assert_eq!(driver.core.draft(fx.a.direct.id), "half a thought");
    assert_eq!(driver.core.draft(fx.b.direct.id), "another");
    assert!(driver.core.members().iter().any(|m| m.id == fx.b.other.id));
    driver.send(Input::SwitchWorkspace(fx.a.workspace.id));
    assert_eq!(driver.core.selected_conversation(), Some(fx.a.direct.id));
    assert!(driver.core.members().iter().any(|m| m.id == fx.a.other.id));
    assert!(!driver.core.members().iter().any(|m| m.id == fx.b.other.id));
}

#[test]
fn nothing_is_cached_under_a_workspace_that_is_not_the_persons() {
    let mut fx = start();
    fx.driver.load_history(fx.a.general.id, vec![], false);
    let cache = fx.driver.last_cache().unwrap();
    for workspace in &cache.workspaces {
        for conversation in &workspace.conversations {
            assert_eq!(conversation.workspace_id, workspace.workspace.id);
        }
        for member in &workspace.members {
            assert!([fx.me.id, fx.a.other.id, fx.b.other.id].contains(&member.id));
            if workspace.workspace.id == fx.a.workspace.id {
                assert_ne!(member.id, fx.b.other.id, "Beta's people are not Alpha's");
            }
        }
    }
}

#[test]
fn the_unused_helpers_are_used() {
    let _ = (EventSeq::ZERO, Timestamp::now());
}

// ----- Answers that arrive after the person moved on -----

#[test]
fn a_settings_answer_is_applied_to_the_workspace_it_was_asked_in() {
    let mut fx = start_with(|_, b| b.direct_level = NotifyLevel::Nothing);
    let mut wanted = *fx.driver.core.notification_settings();
    wanted.direct_level = NotifyLevel::Mentions;
    let out = fx.driver.send(Input::SetNotificationSettings(wanted));
    assert!(out.effects.iter().any(|e| matches!(
        e,
        Effect::SetNotificationSettings { workspace, .. } if *workspace == fx.a.workspace.id
    )));
    fx.switch(fx.b.workspace.id);
    fx.driver.send(Input::NotificationSettingsResult {
        workspace: fx.a.workspace.id,
        result: Ok(lazy_protocol::NotificationSettingsResponse { settings: wanted }),
    });
    assert_eq!(
        fx.driver.core.notification_settings().direct_level,
        NotifyLevel::Nothing,
        "Beta's settings are Beta's"
    );
    fx.switch(fx.a.workspace.id);
    assert_eq!(
        fx.driver.core.notification_settings().direct_level,
        NotifyLevel::Mentions
    );
}

#[test]
fn an_invitation_made_in_one_workspace_never_shows_its_code_in_another() {
    let mut fx = start_with(|a, b| {
        a.role = WorkspaceRole::Admin;
        b.role = WorkspaceRole::Admin;
    });
    let open = |fx: &mut Fx| {
        let out = fx.driver.send(Input::OpenInvitations);
        let generation = out
            .effects
            .iter()
            .find_map(|e| match e {
                Effect::LoadInvitations { generation, .. } => Some(*generation),
                _ => None,
            })
            .expect("asked");
        fx.driver.send(Input::InvitationsLoaded {
            generation,
            result: Ok(lazy_protocol::InvitationList {
                invitations: Vec::new(),
            }),
        });
    };
    open(&mut fx);
    fx.driver.send(Input::CreateInvitation {
        expires_in_secs: None,
        max_uses: None,
    });
    fx.switch(fx.b.workspace.id);
    open(&mut fx);
    let created = lazy_protocol::CreatedInvitation {
        invitation: lazy_protocol::Invitation {
            id: lazy_domain::InvitationId::generate(),
            workspace_id: fx.a.workspace.id,
            created_by: fx.me.id,
            created_at: Timestamp::now(),
            expires_at: Timestamp::now(),
            max_uses: None,
            uses: 0,
            state: lazy_protocol::InvitationState::Active,
        },
        code: InviteCode::new(CODE),
    };
    fx.driver.send(Input::ActionResult {
        kind: ActionKind::CreateInvitation,
        result: Ok(ActionOutcome::InvitationCreated(created)),
    });
    let view = fx.driver.core.invitations();
    assert!(
        view.created.is_none() && view.items.is_empty(),
        "Beta's list is Beta's"
    );
    assert!(!format!("{view:?}").contains(CODE));
    // Alpha's view was closed when it was left, and the code went with it.
    fx.switch(fx.a.workspace.id);
    assert!(fx.driver.core.invitations().created.is_none());
    assert!(!fx.driver.core.invitations().open);
}

#[test]
fn a_channel_joined_after_switching_lands_in_the_workspace_it_belongs_to() {
    let mut fx = start();
    let mut browsed = channel(fx.a.workspace.id, "random");
    browsed.member_count = 7;
    let out = fx.driver.send(Input::JoinChannel(browsed.id));
    let _ = out;
    fx.switch(fx.b.workspace.id);
    fx.driver.send(Input::ActionResult {
        kind: ActionKind::Join(browsed.id),
        result: Ok(ActionOutcome::Conversation(browsed.clone())),
    });
    assert!(
        !fx.driver
            .core
            .conversations()
            .iter()
            .any(|c| c.id == browsed.id),
        "not on Beta's screen"
    );
    fx.switch(fx.a.workspace.id);
    assert!(
        fx.driver
            .core
            .conversations()
            .iter()
            .any(|c| c.id == browsed.id)
    );
}

#[test]
fn a_person_only_the_directory_or_a_profile_knows_has_their_picture_fetched() {
    let mut fx = start();
    let mut stranger = user("stranger");
    stranger.avatar_version = 1;
    let person = |user: &User| lazy_protocol::Person {
        user: user.clone(),
        role: WorkspaceRole::Member,
        joined_at: Timestamp::now(),
    };
    assert!(fx.driver.core.member(stranger.id).is_none());
    assert_eq!(fx.driver.core.avatar_version(stranger.id), 0);
    // Not known anywhere yet: nothing to fetch.
    let out = fx.driver.send(Input::LoadAvatar(stranger.id));
    assert!(fetches(&out).is_empty());

    // A page of the directory lists them.
    let out = fx.driver.send(Input::OpenPeople);
    let generation = out
        .effects
        .iter()
        .find_map(|e| match e {
            Effect::LoadPeople { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("asked");
    fx.driver.send(Input::PeopleLoaded {
        generation,
        result: Ok(lazy_protocol::PeoplePage {
            people: vec![person(&stranger)],
            has_more: false,
        }),
    });
    let out = fx.driver.send(Input::LoadAvatar(stranger.id));
    assert_eq!(fetches(&out), vec![(stranger.id, 1)]);
    assert_eq!(fx.driver.core.avatar_version(stranger.id), 1);
    assert_eq!(fx.driver.core.avatar_version(user("nobody").id), 0);

    // And a profile answer for someone else does the same.
    let mut other = user("another");
    other.avatar_version = 3;
    let out = fx.driver.send(Input::OpenProfile { user_id: other.id });
    let generation = out
        .effects
        .iter()
        .find_map(|e| match e {
            Effect::LoadProfile { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("asked");
    fx.driver.send(Input::ProfileLoaded {
        generation,
        result: Ok(lazy_protocol::PersonResponse {
            person: person(&other),
        }),
    });
    let out = fx.driver.send(Input::LoadAvatar(other.id));
    assert_eq!(fetches(&out), vec![(other.id, 3)]);
}

#[test]
fn selecting_a_conversation_of_another_workspace_switches_to_it_first() {
    let mut fx = start();
    assert_eq!(fx.active(), Some(fx.a.workspace.id));
    fx.driver.send(Input::SelectConversation(fx.b.direct.id));
    assert_eq!(fx.active(), Some(fx.b.workspace.id));
    assert_eq!(fx.driver.core.selected_conversation(), Some(fx.b.direct.id));
    assert!(
        fx.driver
            .core
            .conversations()
            .iter()
            .all(|c| c.workspace_id == fx.b.workspace.id)
    );
    // Nothing is selected in a workspace that is not shown, whatever happens in it.
    fx.driver.send(fx.a.event(
        11,
        Event::ConversationCreated {
            conversation: channel(fx.a.workspace.id, "later"),
        },
    ));
    assert_eq!(fx.driver.core.selected_conversation(), Some(fx.b.direct.id));
}

#[test]
fn the_bytes_of_the_pictures_held_are_bounded_too() {
    let mut fx = start();
    let mut people = Vec::new();
    for n in 0..3 {
        let mut person = user(&format!("heavy{n}"));
        person.avatar_version = 1;
        people.push(person.id);
        fx.driver
            .send(fx.a.event(11 + n, Event::MemberJoined { user: person }));
    }
    let size = lazy_client_core::MAX_HELD_AVATAR_BYTES / 2 - 1024;
    for id in &people {
        let out = fx.driver.send(Input::LoadAvatar(*id));
        for (user, version) in fetches(&out) {
            fx.driver.send(Input::AvatarLoaded {
                user,
                version,
                result: Ok(ImageData::new(vec![0u8; size])),
            });
        }
    }
    let held = people
        .iter()
        .filter(|id| matches!(fx.driver.core.avatar(**id), Some(ImageState::Ready(_))))
        .count();
    assert_eq!(
        held, 2,
        "two fit in the bound, the third pushed the oldest out"
    );
    assert!(matches!(
        fx.driver.core.avatar(people[2]),
        Some(ImageState::Ready(_))
    ));
}

#[test]
fn nothing_is_notified_on_a_guess_before_the_settings_are_known_after_a_restart() {
    let mut fx = start();
    fx.driver.load_history(fx.a.general.id, vec![], false);
    let cache = fx.driver.last_cache().expect("a cache was saved").clone();
    // The app starts again from the cache, which holds no notification settings.
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        cache: Some(cache),
        ..Default::default()
    }));
    driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: fx.a.workspace.id,
        seq: seq(10),
    }));
    let (input, _) = fx.a.direct_message(11, "ping");
    let out = driver.send(input);
    assert!(
        notifications(&out).is_empty(),
        "the settings are not known yet"
    );
    // Once the server has said what they are, messages notify under them.
    driver.send(Input::BootstrapLoaded(Box::new(BootstrapResponse {
        protocol_version: PROTOCOL_VERSION,
        user: fx.me.clone(),
        workspaces: vec![fx.a.bootstrap(&fx.me), fx.b.bootstrap(&fx.me)],
    })));
    driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: fx.a.workspace.id,
        seq: seq(11),
    }));
    let (input, _) = fx.a.direct_message(12, "ping again");
    let out = driver.send(input);
    assert_eq!(notifications(&out).len(), 1, "{:?}", out.effects);
}
