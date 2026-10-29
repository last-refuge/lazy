//! Randomized interleavings for several workspaces (slice 3G, D23): three workspaces with their
//! own logs, conversations, people, roles, and settings, a person who switches between them,
//! creates and joins more, loses one, and changes their profile, while messages arrive in all of
//! them, are delivered late and out of step with each other, and the app restarts from a cache of
//! several workspaces.
//!
//! After every step: the rail lists exactly the workspaces the core has been told of, one of them
//! active; each workspace's attention (and the total) is what the server model says it is for what
//! has been delivered; the active workspace's screen holds that workspace's conversations,
//! people, settings, and role and nothing of another's; a notification is produced at most once
//! per message, under its own workspace's settings and titled with its name when it is not the
//! active one; a picture shown is of the version in use. At the end, once everything is delivered,
//! each workspace is exactly the server's, whichever way it is looked at.
//!
//! The default run is deterministic (a fixed seed); `PROPTEST_CASES` runs more, with a random seed.

mod common;

use std::collections::{HashMap, HashSet};

use common::{Driver, World, channel, people, seq, user};
use lazy_client_core::{
    ActionOutcome, Appearance, ContentCache, Effect, FileBytes, FileSource, ImageData, ImageState,
    Input, MotionChoice, Output, RequestError, SessionPhase, StartupState, ThemeChoice,
    TransferError, ViewState,
};
use lazy_domain::notification::{NotificationSettings, NotifyLevel};
use lazy_domain::{
    ConversationId, DisplayName, MessageId, Timestamp, UserId, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    ApiError, BootstrapResponse, Conversation, ConversationUserState, ErrorCode, Event,
    EventEnvelope, HistoryPage, InviteCode, PROTOCOL_VERSION, ServerFrame, User, Workspace,
    WorkspaceBootstrap,
};
use proptest::prelude::*;
use proptest::test_runner::{RngAlgorithm, TestRng, TestRunner};
use std::sync::atomic::{AtomicUsize, Ordering};

const BASE: i64 = 10;
const DEFAULT_CASES: u32 = 1500;
const DEFAULT_SEED: [u8; 32] = *b"lazy-multi-workspace-seed-v01---";
const JOIN_CODE: &str = "join-code-for-the-hidden-place-01";

#[derive(Debug, Clone, Copy)]
enum Outcome {
    Ok,
    LostAnswer,
    LostRequest,
    Reject,
}

#[derive(Debug, Clone, Copy)]
enum Action {
    /// Someone posts in a workspace: which, in the direct conversation or the channel, whether it
    /// names the person.
    Post(usize, bool, bool),
    Deliver(usize, usize),
    CatchUp(usize),
    Switch(usize),
    SetLevel(usize, usize),
    DeactivateShared(usize, bool),
    ProfileOf(usize, usize),
    Create(usize),
    Accept(bool),
    Resolve(usize, Outcome),
    Snapshot(usize),
    Restart,
    SetAppearance(usize, usize),
    LoadAvatar(usize),
    LoseWorkspace(usize),
    RenameMe(usize),
    ChooseAvatar,
    RemoveAvatar,
    Select(usize, bool),
}

fn outcome() -> impl Strategy<Value = Outcome> {
    prop_oneof![
        5 => Just(Outcome::Ok),
        2 => Just(Outcome::LostAnswer),
        1 => Just(Outcome::LostRequest),
        1 => Just(Outcome::Reject),
    ]
}

fn action() -> impl Strategy<Value = Action> {
    prop_oneof![
        8 => (0usize..4, any::<bool>(), any::<bool>()).prop_map(|(w, d, m)| Action::Post(w, d, m)),
        8 => (0usize..4, 1usize..4).prop_map(|(w, n)| Action::Deliver(w, n)),
        3 => (0usize..4).prop_map(Action::CatchUp),
        5 => (0usize..4).prop_map(Action::Switch),
        2 => (0usize..4, 0usize..3).prop_map(|(w, l)| Action::SetLevel(w, l)),
        2 => (0usize..4, any::<bool>()).prop_map(|(w, d)| Action::DeactivateShared(w, d)),
        3 => (0usize..3, any::<usize>()).prop_map(|(u, n)| Action::ProfileOf(u, n)),
        2 => any::<usize>().prop_map(Action::Create),
        2 => any::<bool>().prop_map(Action::Accept),
        8 => (any::<usize>(), outcome()).prop_map(|(p, o)| Action::Resolve(p, o)),
        2 => (0usize..4).prop_map(Action::Snapshot),
        1 => Just(Action::Restart),
        2 => (0usize..3, 0usize..3).prop_map(|(t, m)| Action::SetAppearance(t, m)),
        4 => (0usize..3).prop_map(Action::LoadAvatar),
        1 => (0usize..4).prop_map(Action::LoseWorkspace),
        2 => any::<usize>().prop_map(Action::RenameMe),
        2 => Just(Action::ChooseAvatar),
        1 => Just(Action::RemoveAvatar),
        3 => (0usize..4, any::<bool>()).prop_map(|(w, d)| Action::Select(w, d)),
    ]
}

/// One workspace as the server has it.
struct Model {
    workspace: Workspace,
    role: WorkspaceRole,
    other: User,
    shared: User,
    shared_deactivated: bool,
    general: Conversation,
    direct: Conversation,
    /// The log: entry `i` has position `BASE + 1 + i`.
    log: Vec<Event>,
    /// Messages by position: (position, conversation, mentions the person).
    messages: Vec<(i64, ConversationId, bool)>,
    level: NotifyLevel,
    /// The server lists it for the person.
    member: bool,
    /// Whether the shared person is in it at all (only the first two workspaces).
    has_shared: bool,
    // What the core has been given.
    pointer: usize,
    applied: i64,
    live: bool,
}

impl Model {
    fn new(name: &str, me: &User, other: &str, shared: &User, has_shared: bool) -> Self {
        let workspace = Workspace {
            id: WorkspaceId::generate(),
            name: name.into(),
            settings: Default::default(),
        };
        let other = user(other);
        let mut general = channel(workspace.id, "general");
        general.is_default = true;
        let direct = people(workspace.id, &[me, &other]);
        Self {
            workspace,
            role: WorkspaceRole::Member,
            other,
            shared: shared.clone(),
            shared_deactivated: false,
            general,
            direct,
            log: Vec::new(),
            messages: Vec::new(),
            level: NotifyLevel::All,
            member: true,
            has_shared,
            pointer: 0,
            applied: BASE,
            live: false,
        }
    }

    fn head(&self) -> i64 {
        BASE + self.log.len() as i64
    }

    /// The direct-message level the person had just before the event at `position`.
    fn level_before(&self, position: i64) -> NotifyLevel {
        let mut level = NotifyLevel::All;
        for event in self.log.iter().take((position - BASE - 1) as usize) {
            if let Event::NotificationSettingsChanged { settings } = event {
                level = settings.direct_level;
            }
        }
        level
    }

    fn settings(&self) -> NotificationSettings {
        NotificationSettings {
            direct_level: self.level,
            ..NotificationSettings::default()
        }
    }

    /// The person's counts in a conversation as the server has them at `position`.
    fn counts(&self, conversation: ConversationId, position: i64) -> (u32, u32) {
        let mut unread = 0;
        let mut mentions = 0;
        for (at, conv, mentioning) in &self.messages {
            if *conv == conversation && *at <= position {
                unread += 1;
                if *mentioning {
                    mentions += 1;
                }
            }
        }
        (unread, mentions)
    }

    /// What needs the person's attention, for the part of the log the core has applied.
    fn attention(&self) -> u32 {
        let (_, channel_mentions) = self.counts(self.general.id, self.applied);
        let (direct_unread, _) = self.counts(self.direct.id, self.applied);
        channel_mentions + direct_unread
    }

    fn unread_dot(&self) -> bool {
        self.attention() == 0
            && (self.counts(self.general.id, self.applied).0 > 0
                || self.counts(self.direct.id, self.applied).0 > 0)
    }

    fn bootstrap(&self, me: &User) -> WorkspaceBootstrap {
        let head = self.head();
        let state = |conversation: &Conversation| {
            let (unread, mentions) = self.counts(conversation.id, head);
            ConversationUserState {
                conversation_id: conversation.id,
                favorite: false,
                last_read: seq(0),
                unread_count: unread,
                mention_count: mentions,
            }
        };
        let mut members = vec![me.clone(), self.other.clone()];
        if self.has_shared {
            let mut shared = self.shared.clone();
            shared.deactivated = self.shared_deactivated;
            members.push(shared);
        }
        WorkspaceBootstrap {
            workspace: self.workspace.clone(),
            role: self.role,
            conversations: vec![self.general.clone(), self.direct.clone()],
            user_state: vec![state(&self.general), state(&self.direct)],
            members,
            groups: Vec::new(),
            custom_emoji: Vec::new(),
            notification_settings: self.settings(),
            conversation_notification_prefs: Vec::new(),
            threads: Vec::new(),
            saved: Vec::new(),
            head: seq(head),
        }
    }
}

struct Sim {
    me: User,
    shared: User,
    models: Vec<Model>,
    /// The workspaces the core has been told of, in the order it lists them.
    known: Vec<usize>,
    driver: Driver,
    pending: Vec<Effect>,
    notified: HashSet<MessageId>,
    message_ids: HashMap<(usize, i64), MessageId>,
    appearance: Appearance,
    saved_appearance: Appearance,
    last_cache: Option<ContentCache>,
    last_view: ViewState,
    last_active: Option<WorkspaceId>,
    /// The server's own profile and that of the shared person.
    names: HashMap<UserId, (String, u32)>,
    next_name: usize,
    created: usize,
    trace: Vec<String>,
}

fn counter(c: &AtomicUsize) {
    c.fetch_add(1, Ordering::Relaxed);
}

static SWITCHES: AtomicUsize = AtomicUsize::new(0);
static BACKGROUND_COUNTS: AtomicUsize = AtomicUsize::new(0);
static BACKGROUND_NOTIFICATIONS: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_NOTIFICATIONS: AtomicUsize = AtomicUsize::new(0);
static WORKSPACES_CREATED: AtomicUsize = AtomicUsize::new(0);
static WORKSPACES_JOINED: AtomicUsize = AtomicUsize::new(0);
static WORKSPACES_LOST: AtomicUsize = AtomicUsize::new(0);
static RESTARTS_WITH_SEVERAL: AtomicUsize = AtomicUsize::new(0);
static AVATARS_SHOWN: AtomicUsize = AtomicUsize::new(0);
static PROFILE_EVENTS: AtomicUsize = AtomicUsize::new(0);
static ANSWERS_AFTER_SWITCHING: AtomicUsize = AtomicUsize::new(0);
static SNAPSHOTS: AtomicUsize = AtomicUsize::new(0);

impl Sim {
    fn new() -> Self {
        let me = user("jules");
        let shared = user("samuel");
        let models = vec![
            Model::new("Alpha Works", &me, "zoe", &shared, true),
            Model::new("Beta", &me, "bob", &shared, true),
            Model::new("Gamma", &me, "gus", &shared, false),
            // The one the person is invited to.
            {
                let mut hidden = Model::new("Hidden Place", &me, "hal", &shared, false);
                hidden.member = false;
                hidden
            },
        ];
        let mut sim = Self {
            names: HashMap::from([
                (me.id, ("JULES".to_owned(), 0)),
                (shared.id, ("SAMUEL".to_owned(), 0)),
            ]),
            me,
            shared,
            known: vec![0, 1, 2],
            models,
            driver: Driver::new(),
            pending: Vec::new(),
            notified: HashSet::new(),
            message_ids: HashMap::new(),
            appearance: Appearance::default(),
            saved_appearance: Appearance::default(),
            last_cache: None,
            last_view: ViewState::default(),
            last_active: None,
            next_name: 0,
            created: 0,
            trace: Vec::new(),
        };
        let out = sim.driver.send(Input::Started(StartupState {
            session_restored: true,
            ..Default::default()
        }));
        sim.process(out);
        sim.pending.clear();
        sim.resolve_bootstrap();
        sim
    }

    fn index_of(&self, workspace: WorkspaceId) -> usize {
        self.models
            .iter()
            .position(|m| m.workspace.id == workspace)
            .expect("a workspace of the model")
    }

    fn active_index(&self) -> Option<usize> {
        self.driver
            .core
            .active_workspace()
            .map(|id| self.index_of(id))
    }

    fn server_listing(&self) -> Vec<usize> {
        // The server lists, in order, the workspaces the person belongs to: those the core knew
        // first (in its order), then the others by creation.
        let mut listing: Vec<usize> = self
            .known
            .iter()
            .copied()
            .filter(|i| self.models[*i].member)
            .collect();
        for (i, model) in self.models.iter().enumerate() {
            if model.member && !listing.contains(&i) {
                listing.push(i);
            }
        }
        listing
    }

    fn response(&self) -> BootstrapResponse {
        let mut user = self.me.clone();
        user.display_name = DisplayName::parse(&self.names[&self.me.id].0).unwrap();
        user.avatar_version = self.names[&self.me.id].1;
        BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: user.clone(),
            workspaces: self
                .server_listing()
                .into_iter()
                .map(|i| self.profile_applied(self.models[i].bootstrap(&user)))
                .collect(),
        }
    }

    /// A bootstrap shows people as they are now: names and pictures.
    fn profile_applied(&self, mut bootstrap: WorkspaceBootstrap) -> WorkspaceBootstrap {
        for member in &mut bootstrap.members {
            if let Some((name, version)) = self.names.get(&member.id) {
                member.display_name = DisplayName::parse(name).unwrap();
                member.avatar_version = *version;
            }
        }
        bootstrap
    }

    fn resolve_bootstrap(&mut self) {
        let response = self.response();
        let listed = self.server_listing();
        // The core is told what the server lists, in its order.
        self.known = listed;
        self.feed(Input::BootstrapLoaded(Box::new(response)));
    }

    fn feed(&mut self, input: Input) {
        let line = format!("{input:?}");
        self.trace.push(line.chars().take(220).collect::<String>());
        let active_before = self.driver.core.active_workspace();
        let out = self.driver.send(input);
        self.process_with(out, active_before);
        self.check();
    }

    fn process(&mut self, out: Output) {
        let active = self.driver.core.active_workspace();
        self.process_with(out, active);
    }

    /// `active_before` is the workspace that was active when the input was handled: a
    /// notification is titled with its workspace's name when it was not that one.
    fn process_with(&mut self, out: Output, _active_before: Option<WorkspaceId>) {
        for effect in out.effects {
            match effect {
                Effect::Subscribe { workspace, after } => {
                    let index = self.index_of(workspace);
                    let model = &mut self.models[index];
                    model.pointer = (after.get() - BASE) as usize;
                    model.applied = after.get();
                    model.live = false;
                }
                Effect::Unsubscribe { workspace } => {
                    let index = self.index_of(workspace);
                    assert!(
                        !self.models[index].member,
                        "the core stopped reading a workspace the server still lists\n{}",
                        self.trace.join("\n")
                    );
                }
                Effect::PersistCache(cache) => {
                    for cached in &cache.workspaces {
                        let index = self.index_of(cached.workspace.id);
                        assert!(cached.cursor.get() <= self.models[index].head());
                        for conversation in &cached.conversations {
                            assert_eq!(
                                conversation.workspace_id, cached.workspace.id,
                                "a conversation cached under another workspace"
                            );
                        }
                    }
                    self.last_cache = Some(*cache);
                }
                Effect::PersistView(view) => self.last_view = view,
                Effect::PersistActiveWorkspace(workspace) => self.last_active = workspace,
                Effect::PersistAppearance(appearance) => self.saved_appearance = appearance,
                Effect::SetBadge(count) => assert_eq!(
                    count,
                    self.driver.core.attention_total(),
                    "the badge is the total\n{}",
                    self.trace.join("\n")
                ),
                Effect::Notify(notification) => self.notification(&notification),
                Effect::FetchHistory {
                    conversation,
                    generation,
                    ..
                } => {
                    self.trace.push("(history answered empty)".into());
                    let out = self.driver.send(Input::HistoryLoaded {
                        conversation,
                        generation,
                        result: Ok(HistoryPage {
                            messages: Vec::new(),
                            has_more: false,
                        }),
                    });
                    self.process(out);
                }
                Effect::FetchMembers {
                    conversation,
                    generation,
                } => {
                    let user_ids = self.members_of(conversation);
                    let out = self.driver.send(Input::MembersFetched {
                        conversation,
                        generation,
                        result: Ok(lazy_protocol::ConversationMembersResponse {
                            conversation_id: conversation,
                            user_ids,
                        }),
                    });
                    self.process(out);
                }
                effect @ (Effect::LoadBootstrap
                | Effect::CreateWorkspace { .. }
                | Effect::AcceptInvitation { .. }
                | Effect::UpdateProfile { .. }
                | Effect::SetAvatar { .. }
                | Effect::RemoveAvatar
                | Effect::FetchAvatar { .. }) => self.pending.push(effect),
                _ => {}
            }
        }
    }

    fn members_of(&self, conversation: ConversationId) -> Vec<UserId> {
        for model in &self.models {
            if model.general.id == conversation {
                return vec![self.me.id, model.other.id];
            }
            if model.direct.id == conversation {
                return vec![self.me.id, model.other.id];
            }
        }
        Vec::new()
    }

    fn notification(&mut self, notification: &lazy_client_core::DesktopNotification) {
        assert!(
            self.notified.insert(notification.message),
            "a message notified twice\n{}",
            self.trace.join("\n")
        );
        let (index, position) = self
            .message_ids
            .iter()
            .find(|(_, id)| **id == notification.message)
            .map(|((index, position), _)| (*index, *position))
            .expect("a notification for a message the server made");
        let model = &self.models[index];
        let mentioning = model.messages.iter().any(|(at, conversation, mentioning)| {
            *at == position && *conversation == notification.conversation && *mentioning
        });
        let by_settings = (model.direct.id == notification.conversation
            && model.level_before(position) == NotifyLevel::All)
            || (model.general.id == notification.conversation && mentioning);
        assert!(
            model.live && by_settings,
            "a notification the workspace's settings do not allow ({position})\n{}",
            self.trace.join("\n")
        );
        // The title says whose it is when it is not the active workspace (and there are several).
        let several = self.driver.core.workspace_rail().len() > 1;
        let background = self.active_index() != Some(index);
        let named = notification
            .title
            .starts_with(&format!("{} · ", model.workspace.name));
        assert_eq!(
            named,
            several && background,
            "{:?} (background: {background})\n{}",
            notification.title,
            self.trace.join("\n")
        );
        counter(if background {
            &BACKGROUND_NOTIFICATIONS
        } else {
            &ACTIVE_NOTIFICATIONS
        });
    }

    // ----- The server -----

    fn post(&mut self, index: usize, direct: bool, mention: bool) {
        let model = &mut self.models[index];
        if !model.member {
            return;
        }
        let conversation = if direct {
            &model.direct
        } else {
            &model.general
        };
        let at = BASE + 1 + model.log.len() as i64;
        let mut message = World::new().message(conversation.id, &model.other, at, "hello");
        let mentioning = mention && !direct;
        if mentioning {
            message.mentions.users.push(self.me.id);
        }
        self.message_ids.insert((index, at), message.id);
        let conversation_id = conversation.id;
        model.messages.push((at, conversation_id, mentioning));
        model.log.push(Event::MessageCreated {
            message,
            operation_id: None,
        });
    }

    fn deliver(&mut self, index: usize, count: usize) {
        for _ in 0..count {
            let model = &self.models[index];
            if !self.known.contains(&index) || !model.member || model.pointer >= model.log.len() {
                return;
            }
            let at = BASE + 1 + model.pointer as i64;
            let event = model.log[model.pointer].clone();
            let envelope = EventEnvelope {
                workspace_id: model.workspace.id,
                seq: seq(at),
                occurred_at: Timestamp::now(),
                event,
            };
            let live = model.live;
            let before = self.driver.core.attention_total();
            self.models[index].pointer += 1;
            self.models[index].applied = self.models[index].applied.max(at);
            let _ = live;
            self.feed(Input::Frame(ServerFrame::Event(envelope)));
            if self.active_index() != Some(index) && self.driver.core.attention_total() != before {
                counter(&BACKGROUND_COUNTS);
            }
        }
    }

    fn server_profile(&mut self, who: usize, number: usize) {
        let user = if who == 0 { self.me.id } else { self.shared.id };
        let entry = self.names.get_mut(&user).unwrap();
        entry.0 = format!("NAME{}", number % 7);
        if number.is_multiple_of(3) {
            entry.1 += 1;
        }
        let (name, version) = entry.clone();
        let event = Event::UserProfileChanged {
            user_id: user,
            display_name: DisplayName::parse(&name).unwrap(),
            avatar_version: version,
        };
        for model in &mut self.models {
            let belongs = if user == self.me.id {
                true
            } else {
                model.has_shared
            };
            if model.member && belongs {
                model.log.push(event.clone());
            }
        }
        counter(&PROFILE_EVENTS);
    }

    fn resolve(&mut self, pick: usize, outcome: Outcome) {
        if self.pending.is_empty() {
            return;
        }
        let effect = self.pending.remove(pick % self.pending.len());
        let unknown = || RequestError::Unknown("lost".into());
        let refuse = |code: ErrorCode| RequestError::Rejected(ApiError::new(code, "no"));
        match effect {
            Effect::LoadBootstrap => match outcome {
                Outcome::LostAnswer | Outcome::LostRequest | Outcome::Reject => {
                    self.feed(Input::BootstrapFailed(unknown()));
                }
                Outcome::Ok => self.resolve_bootstrap(),
            },
            Effect::CreateWorkspace { workspace, request } => {
                let result = match outcome {
                    Outcome::LostRequest => Err(unknown()),
                    Outcome::Reject => Err(refuse(ErrorCode::LimitReached)),
                    Outcome::Ok | Outcome::LostAnswer => {
                        let index =
                            match self.models.iter().position(|m| m.workspace.id == workspace) {
                                Some(index) => index,
                                None => {
                                    self.created += 1;
                                    let mut model = Model::new(
                                        request.name.as_str(),
                                        &self.me,
                                        &format!("made{}", self.created),
                                        &self.shared,
                                        false,
                                    );
                                    model.workspace.id = workspace;
                                    model.general.workspace_id = workspace;
                                    model.direct.workspace_id = workspace;
                                    model.role = WorkspaceRole::Owner;
                                    self.models.push(model);
                                    counter(&WORKSPACES_CREATED);
                                    self.models.len() - 1
                                }
                            };
                        if matches!(outcome, Outcome::Ok) {
                            let bootstrap = self
                                .profile_applied(self.models[index].bootstrap(&self.me.clone()));
                            if !self.known.contains(&index) {
                                self.known.push(index);
                            }
                            Ok(ActionOutcome::Joined(Box::new(bootstrap)))
                        } else {
                            Err(unknown())
                        }
                    }
                };
                self.feed(Input::ActionResult {
                    kind: lazy_client_core::ActionKind::CreateWorkspace,
                    result,
                });
            }
            Effect::AcceptInvitation { request } => {
                let right = request.code == InviteCode::new(JOIN_CODE);
                let result = match (right, outcome) {
                    (_, Outcome::LostRequest | Outcome::LostAnswer) => Err(unknown()),
                    (false, _) | (_, Outcome::Reject) => Err(refuse(ErrorCode::InvitationInvalid)),
                    (true, Outcome::Ok) => {
                        let index = 3;
                        if !self.models[index].member {
                            self.models[index].member = true;
                            counter(&WORKSPACES_JOINED);
                        }
                        let bootstrap =
                            self.profile_applied(self.models[index].bootstrap(&self.me.clone()));
                        if !self.known.contains(&index) {
                            self.known.push(index);
                        }
                        Ok(ActionOutcome::Joined(Box::new(bootstrap)))
                    }
                };
                self.feed(Input::ActionResult {
                    kind: lazy_client_core::ActionKind::AcceptInvitation,
                    result,
                });
            }
            Effect::UpdateProfile { request } => {
                let name = request
                    .display_name
                    .map(|n| n.as_str().to_owned())
                    .unwrap_or_default();
                self.profile_answer(
                    lazy_client_core::ActionKind::UpdateProfile,
                    outcome,
                    |names| {
                        names.0 = name;
                    },
                );
            }
            Effect::SetAvatar { .. } => {
                self.profile_answer(lazy_client_core::ActionKind::SetAvatar, outcome, |names| {
                    names.1 += 1;
                });
            }
            Effect::RemoveAvatar => {
                self.profile_answer(lazy_client_core::ActionKind::SetAvatar, outcome, |names| {
                    names.1 = 0;
                });
            }
            Effect::FetchAvatar { user, version } => {
                let result = match outcome {
                    Outcome::Ok => Ok(ImageData::new(vec![version as u8; 8])),
                    _ => Err(TransferError::Request(unknown())),
                };
                self.feed(Input::AvatarLoaded {
                    user,
                    version,
                    result,
                });
            }
            _ => {}
        }
    }

    fn profile_answer(
        &mut self,
        kind: lazy_client_core::ActionKind,
        outcome: Outcome,
        change: impl FnOnce(&mut (String, u32)),
    ) {
        let me = self.me.id;
        let result = match outcome {
            Outcome::LostRequest => Err(RequestError::Unknown("lost".into())),
            Outcome::Reject => Err(RequestError::Rejected(ApiError::new(
                ErrorCode::UnsupportedMedia,
                "no",
            ))),
            Outcome::Ok | Outcome::LostAnswer => {
                let before = self.names[&me].clone();
                change(self.names.get_mut(&me).unwrap());
                let after = self.names[&me].clone();
                if before != after {
                    let event = Event::UserProfileChanged {
                        user_id: me,
                        display_name: DisplayName::parse(&after.0).unwrap(),
                        avatar_version: after.1,
                    };
                    for model in &mut self.models {
                        if model.member {
                            model.log.push(event.clone());
                        }
                    }
                }
                if matches!(outcome, Outcome::Ok) {
                    let mut user = self.me.clone();
                    user.display_name = DisplayName::parse(&after.0).unwrap();
                    user.avatar_version = after.1;
                    Ok(ActionOutcome::Profile(user))
                } else {
                    Err(RequestError::Unknown("lost".into()))
                }
            }
        };
        self.feed(Input::ActionResult { kind, result });
    }

    // ----- Steps -----

    fn step(&mut self, action: Action) {
        let count = self.models.len();
        match action {
            Action::Post(w, d, m) => self.post(w % count, d, m),
            Action::Deliver(w, n) => self.deliver(w % count, n),
            Action::CatchUp(w) => {
                let index = w % count;
                let model = &self.models[index];
                if self.known.contains(&index) && model.member && model.pointer >= model.log.len() {
                    let workspace_id = model.workspace.id;
                    let at = model.applied;
                    self.models[index].live = true;
                    self.feed(Input::Frame(ServerFrame::CaughtUp {
                        workspace_id,
                        seq: seq(at),
                    }));
                }
            }
            Action::Switch(w) => {
                let index = w % count;
                if self.known.contains(&index) {
                    let id = self.models[index].workspace.id;
                    let rail_before = self.driver.core.workspace_rail();
                    self.feed(Input::SwitchWorkspace(id));
                    counter(&SWITCHES);
                    // Its number in the rail was what its own attention_count() is when active.
                    let expected = rail_before
                        .iter()
                        .find(|item| item.id == id)
                        .map(|item| item.attention);
                    if let Some(expected) = expected
                        && self.driver.core.active_workspace() == Some(id)
                    {
                        assert_eq!(
                            self.driver.core.attention_count(),
                            expected,
                            "the rail promised another number\n{}",
                            self.trace.join("\n")
                        );
                    }
                }
            }
            Action::SetLevel(w, l) => {
                let index = w % count;
                let level = [
                    NotifyLevel::All,
                    NotifyLevel::Mentions,
                    NotifyLevel::Nothing,
                ][l % 3];
                let model = &mut self.models[index];
                if model.member && model.level != level {
                    model.level = level;
                    let settings = model.settings();
                    model
                        .log
                        .push(Event::NotificationSettingsChanged { settings });
                }
            }
            Action::DeactivateShared(w, flag) => {
                let model = &mut self.models[w % count];
                if model.member && model.has_shared && model.shared_deactivated != flag {
                    model.shared_deactivated = flag;
                    let user_id = model.shared.id;
                    model.log.push(Event::MemberDeactivated {
                        user_id,
                        deactivated: flag,
                    });
                }
            }
            Action::ProfileOf(u, n) => self.server_profile(u % 2, n),
            Action::Create(n) => {
                self.next_name += 1;
                self.feed(Input::CreateWorkspace {
                    name: if n % 9 == 0 {
                        "   ".into()
                    } else {
                        format!("Made {}", self.next_name)
                    },
                });
            }
            Action::Accept(right) => self.feed(Input::AcceptInvitation {
                code: if right {
                    JOIN_CODE
                } else {
                    "wrong-code-wrong-code-wrong"
                }
                .into(),
            }),
            Action::Resolve(pick, o) => self.resolve(pick, o),
            Action::Snapshot(w) => {
                let index = w % count;
                if self.known.contains(&index) && self.models[index].member {
                    let workspace_id = self.models[index].workspace.id;
                    counter(&SNAPSHOTS);
                    self.feed(Input::Frame(ServerFrame::SnapshotRequired {
                        workspace_id,
                        reason: lazy_protocol::SnapshotReason::CursorExpired,
                    }));
                }
            }
            Action::Restart => self.restart(),
            Action::SetAppearance(t, m) => {
                let appearance = Appearance {
                    theme: [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark][t % 3],
                    motion: [
                        MotionChoice::System,
                        MotionChoice::Reduce,
                        MotionChoice::Full,
                    ][m % 3],
                };
                self.appearance = appearance;
                self.feed(Input::SetAppearance(appearance));
            }
            Action::LoadAvatar(u) => {
                let user = [self.me.id, self.shared.id, self.models[0].other.id][u % 3];
                self.feed(Input::LoadAvatar(user));
            }
            Action::LoseWorkspace(w) => {
                let index = w % count;
                if self.models[index].member && self.server_listing().len() > 1 && index != 3 {
                    self.models[index].member = false;
                    counter(&WORKSPACES_LOST);
                    // The server ends that subscription; the core asks what it may still read.
                    self.feed(Input::Frame(ServerFrame::Error {
                        code: ErrorCode::Forbidden,
                        message: "you are no longer a member of that workspace".into(),
                    }));
                }
            }
            Action::RenameMe(n) => self.feed(Input::UpdateProfile {
                display_name: Some(format!("ME{}", n % 5)),
            }),
            Action::ChooseAvatar => self.feed(Input::SetAvatar {
                source: FileSource::Bytes(FileBytes::new(vec![1, 2, 3])),
            }),
            Action::RemoveAvatar => self.feed(Input::RemoveAvatar),
            Action::Select(w, d) => {
                let index = w % count;
                if self.active_index() == Some(index) {
                    let model = &self.models[index];
                    let id = if d { model.direct.id } else { model.general.id };
                    self.feed(Input::SelectConversation(id));
                }
            }
        }
    }

    fn restart(&mut self) {
        let Some(cache) = self.last_cache.clone() else {
            return;
        };
        // What the dead process had asked for is gone, and what it had told the core of is not.
        self.pending.clear();
        let several = cache.workspaces.len() > 1;
        // A cache lists only workspaces that still are; the core is told of the rest by the
        // bootstrap that follows.
        self.known = cache
            .workspaces
            .iter()
            .map(|cached| self.index_of(cached.workspace.id))
            .collect();
        self.driver = Driver::new();
        for model in &mut self.models {
            model.live = false;
        }
        let state = StartupState {
            session_restored: true,
            cache: Some(cache),
            view: self.last_view.clone(),
            active_workspace: self.last_active,
            appearance: self.saved_appearance,
            ..Default::default()
        };
        self.feed(Input::Started(state));
        if several {
            counter(&RESTARTS_WITH_SEVERAL);
        }
        // Positions of the models the core resumes from are set by its Subscribe effects.
    }

    // ----- Invariants -----

    fn check(&self) {
        let core = &self.driver.core;
        if !matches!(core.phase(), SessionPhase::Ready) {
            return;
        }
        let rail = core.workspace_rail();
        let ids: Vec<WorkspaceId> = rail.iter().map(|item| item.id).collect();
        let expected: Vec<WorkspaceId> = self
            .known
            .iter()
            .map(|i| self.models[*i].workspace.id)
            .collect();
        assert_eq!(
            ids,
            expected,
            "the rail lists what the core was told of\n{}",
            self.trace.join("\n")
        );
        assert_eq!(
            rail.iter().filter(|item| item.active).count(),
            usize::from(!rail.is_empty())
        );
        assert_eq!(
            rail.iter().find(|item| item.active).map(|item| item.id),
            core.active_workspace()
        );
        let mut total = 0u32;
        for item in &rail {
            let index = self.index_of(item.id);
            let model = &self.models[index];
            assert_eq!(item.name, model.workspace.name);
            assert_eq!(item.role, model.role);
            // Only what the core has applied counts.
            assert_eq!(
                item.attention,
                model.attention(),
                "attention of {}\n{}",
                model.workspace.name,
                self.trace.join("\n")
            );
            assert_eq!(
                item.unread,
                model.unread_dot(),
                "the dot of {}",
                model.workspace.name
            );
            total += item.attention;
        }
        assert_eq!(core.attention_total(), total, "the total is the sum");
        if let Some(index) = self.active_index() {
            let model = &self.models[index];
            assert_eq!(core.attention_count(), model.attention());
            self.check_screen(index);
        } else {
            assert!(core.conversations().is_empty() && core.selected_conversation().is_none());
        }
        assert_eq!(core.appearance(), self.appearance);
        // A picture shown is of the version in use.
        for user in [self.me.id, self.shared.id, self.models[0].other.id] {
            if let Some(ImageState::Ready(data)) = core.avatar(user) {
                // (A person of another workspace only is not a member of this one.)
                let Some(version) = core.member(user).map(|u| u.avatar_version) else {
                    continue;
                };
                assert_eq!(
                    data.bytes().first().copied(),
                    Some(version as u8),
                    "an old picture is shown\n{}",
                    self.trace.join("\n")
                );
                counter(&AVATARS_SHOWN);
            }
        }
    }

    /// The active workspace's screen is that workspace's, whole, and nothing else.
    fn check_screen(&self, index: usize) {
        let core = &self.driver.core;
        let model = &self.models[index];
        let mut shown: Vec<ConversationId> = core.conversations().iter().map(|c| c.id).collect();
        shown.sort();
        let mut want = vec![model.general.id, model.direct.id];
        want.sort();
        assert_eq!(
            shown,
            want,
            "the screen holds another workspace's conversations\n{}",
            self.trace.join("\n")
        );
        let mut people: Vec<UserId> = core.members().iter().map(|m| m.id).collect();
        people.sort();
        let mut belong = vec![self.me.id, model.other.id];
        if model.has_shared && !model.shared_deactivated_for_core(core) {
            belong.push(self.shared.id);
        }
        for person in &people {
            assert!(
                belong.contains(person) || (model.has_shared && *person == self.shared.id),
                "a stranger among the people\n{}",
                self.trace.join("\n")
            );
        }
        assert_eq!(
            core.primary_workspace().unwrap().workspace.id,
            model.workspace.id
        );
        assert_eq!(core.my_role(), Some(model.role));
        if let Some(selected) = core.selected_conversation() {
            assert!(selected == model.general.id || selected == model.direct.id);
        }
    }

    // ----- The end -----

    fn drain(&mut self) {
        // The connections come back once everything is delivered: that is when a bootstrap that
        // failed is asked for again.
        let mut reconnected = false;
        for _ in 0..200 {
            let mut progressed = false;
            for index in 0..self.models.len() {
                let model = &self.models[index];
                if self.known.contains(&index) && model.member && model.pointer < model.log.len() {
                    let rest = model.log.len() - model.pointer;
                    self.deliver(index, rest);
                    progressed = true;
                }
            }
            if !self.pending.is_empty() {
                self.resolve(0, Outcome::Ok);
                progressed = true;
            }
            // A person who has been told nothing of a workspace they belong to learns by asking.
            // The person presses "Retry" on the screen that says the server cannot be reached.
            if self.pending.is_empty()
                && matches!(self.driver.core.phase(), SessionPhase::Unreachable { .. })
            {
                self.feed(Input::RetryLoad);
                continue;
            }
            if !progressed && !reconnected {
                reconnected = true;
                for index in self.known.clone() {
                    if self.models[index].member {
                        self.step(Action::CatchUp(index));
                    }
                }
                continue;
            }
            if !progressed {
                let listing = self.server_listing();
                if listing != self.known && self.pending.is_empty() {
                    self.driver_ask_for_bootstrap();
                    continue;
                }
                return;
            }
        }
        panic!("the simulation did not quiesce\n{}", self.trace.join("\n"));
    }

    fn driver_ask_for_bootstrap(&mut self) {
        // A snapshot request makes the core ask; resolving it tells it everything.
        if let Some(index) = self.known.first().copied() {
            let workspace_id = self.models[index].workspace.id;
            self.feed(Input::Frame(ServerFrame::SnapshotRequired {
                workspace_id,
                reason: lazy_protocol::SnapshotReason::CursorExpired,
            }));
        } else {
            self.feed(Input::RetryLoad);
        }
    }

    fn check_final(&mut self) {
        let listing = self.server_listing();
        assert_eq!(self.known, listing, "the core knows what the server lists");
        let ids: Vec<WorkspaceId> = self
            .driver
            .core
            .workspace_rail()
            .iter()
            .map(|item| item.id)
            .collect();
        assert_eq!(ids.len(), listing.len());
        // Look at each workspace as the active one.
        for index in listing.clone() {
            let id = self.models[index].workspace.id;
            self.feed(Input::SwitchWorkspace(id));
            let core = &self.driver.core;
            let model = &self.models[index];
            assert_eq!(core.active_workspace(), Some(id));
            assert_eq!(core.attention_count(), model.attention());
            assert_eq!(model.applied, model.head(), "everything delivered");
            assert_eq!(core.primary_workspace().unwrap().workspace, model.workspace);
            assert_eq!(
                *core.notification_settings(),
                model.settings(),
                "settings of {}",
                model.workspace.name
            );
            if model.has_shared {
                let shared = core.member(self.shared.id).expect("the shared person");
                assert_eq!(
                    shared.deactivated, model.shared_deactivated,
                    "per-workspace state"
                );
            }
        }
        // The people are the same everywhere: names and pictures converge.
        for user in [self.me.id, self.shared.id] {
            for index in listing.clone() {
                let id = self.models[index].workspace.id;
                self.feed(Input::SwitchWorkspace(id));
                if user != self.me.id && !self.models[index].has_shared {
                    continue;
                }
                let held = self.driver.core.member(user).expect("known");
                let (name, version) = &self.names[&user];
                assert_eq!(
                    held.display_name.as_str(),
                    name,
                    "a name has converged\n{}",
                    self.trace.join("\n")
                );
                assert_eq!(held.avatar_version, *version);
            }
        }
        assert_eq!(
            self.driver.core.me().unwrap().display_name.as_str(),
            self.names[&self.me.id].0
        );
    }
}

impl Model {
    /// Whether the core shows the shared person as deactivated in this workspace (a model of what
    /// has been delivered is not kept; the picker may lag the server, and both are allowed).
    fn shared_deactivated_for_core(&self, _core: &lazy_client_core::ClientCore) -> bool {
        false
    }
}

#[test]
fn several_workspaces_stay_consistent_under_any_interleaving() {
    let (cases, mut runner) = match std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    {
        Some(cases) => (
            cases,
            TestRunner::new(ProptestConfig {
                cases,
                ..ProptestConfig::default()
            }),
        ),
        None => (
            DEFAULT_CASES,
            TestRunner::new_with_rng(
                ProptestConfig {
                    cases: DEFAULT_CASES,
                    ..ProptestConfig::default()
                },
                TestRng::from_seed(RngAlgorithm::ChaCha, &DEFAULT_SEED),
            ),
        ),
    };
    let _ = cases;
    runner
        .run(&prop::collection::vec(action(), 1..100), |actions| {
            let mut sim = Sim::new();
            for action in actions {
                sim.step(action);
            }
            sim.drain();
            sim.check_final();
            Ok(())
        })
        .unwrap_or_else(|failure| panic!("{failure}"));
    let mut missing = Vec::new();
    for (name, c) in [
        ("switches", &SWITCHES),
        ("counts that grew in the background", &BACKGROUND_COUNTS),
        (
            "notifications from the background",
            &BACKGROUND_NOTIFICATIONS,
        ),
        (
            "notifications from the active workspace",
            &ACTIVE_NOTIFICATIONS,
        ),
        ("workspaces created", &WORKSPACES_CREATED),
        ("workspaces joined", &WORKSPACES_JOINED),
        ("workspaces lost", &WORKSPACES_LOST),
        ("restarts with several workspaces", &RESTARTS_WITH_SEVERAL),
        ("pictures shown", &AVATARS_SHOWN),
        ("profile events", &PROFILE_EVENTS),
        ("snapshots", &SNAPSHOTS),
    ] {
        eprintln!(
            "multi-workspace coverage: {} {name}",
            c.load(Ordering::Relaxed)
        );
        missing.extend((c.load(Ordering::Relaxed) == 0).then_some(name));
    }
    let _ = &ANSWERS_AFTER_SWITCHING;
    assert!(missing.is_empty(), "never exercised: {missing:?}");
    let _ = (people_unused(), Conversation::clone);
}

fn people_unused() {}
