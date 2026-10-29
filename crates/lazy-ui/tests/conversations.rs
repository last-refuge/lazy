//! Conversation flows on GPUI's test platform: the real views, the real core, and a recording
//! effect sink standing in for the runtime. Each test drives real clicks and key presses, answers
//! the effects the core emits the way the runtime would, and checks what the person would see
//! (through labelled elements) and what the core was asked to do.
//!
//! The test platform's window never reports itself active and gives every character the same
//! width, so these prove behavior and wiring, not real layout (see the offscreen renders for that)
//! and not real window activation.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, Modifiers, TestAppContext, VisualTestContext, WindowAppearance,
};
use lazy_client_core::{
    ActionKind, ActionOutcome, ActionState, Effect, Input, RequestError, StartupState,
};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    MessageDocument, MessageId, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    ApiError, BootstrapResponse, BrowsedChannel, Conversation, ConversationResponse,
    ConversationUserState, ErrorCode, Event, EventEnvelope, HistoryPage, Message, PROTOCOL_VERSION,
    ServerFrame, User, Workspace, WorkspaceBootstrap,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::root_view::RootView;

#[derive(Default)]
struct Recorder(RefCell<Vec<Effect>>);

impl EffectSink for Recorder {
    fn execute(&self, effects: Vec<Effect>) {
        self.0.borrow_mut().extend(effects);
    }
}

struct World {
    model: Entity<AppModel>,
    effects: Rc<Recorder>,
    me: User,
    bob: User,
    zoe: User,
    carol: User,
    workspace: WorkspaceId,
    general: ConversationId,
    design: ConversationId,
    dm_bob: ConversationId,
    group: ConversationId,
}

fn person(name: &str) -> User {
    User {
        is_bot: false,
        deactivated: false,
        avatar_version: 0,
        status: None,
        id: UserId::generate(),
        username: Username::parse(name).unwrap(),
        display_name: DisplayName::parse(&format!("User {name}")).unwrap(),
    }
}

fn start(cx: &mut TestAppContext) -> (World, &mut VisualTestContext) {
    cx.update(|cx| lazy_ui::init(cx, WindowAppearance::Light));
    let effects = Rc::new(Recorder::default());
    let model = cx.new(|_| AppModel::new(effects.clone()));
    let (_root, cx) = cx.add_window_view({
        let model = model.clone();
        move |window, cx| RootView::new(model, window, cx)
    });
    let world = World {
        model,
        effects,
        me: person("alice"),
        bob: person("bob"),
        zoe: person("zoe"),
        carol: person("carol"),
        workspace: WorkspaceId::generate(),
        general: ConversationId::generate(),
        design: ConversationId::generate(),
        dm_bob: ConversationId::generate(),
        group: ConversationId::generate(),
    };
    (world, cx)
}

fn state(
    conversation: ConversationId,
    favorite: bool,
    last_read: i64,
    unread: u32,
    mentions: u32,
) -> ConversationUserState {
    ConversationUserState {
        conversation_id: conversation,
        favorite,
        last_read: EventSeq::new(last_read).unwrap(),
        unread_count: unread,
        mention_count: mentions,
    }
}

impl World {
    fn channel(&self, id: ConversationId, name: &str, creator: Option<UserId>) -> Conversation {
        Conversation {
            id,
            workspace_id: self.workspace,
            kind: ConversationKind::Channel,
            name: Some(ChannelName::parse(name).unwrap()),
            topic: Some(format!("About {name}")),
            description: None,
            visibility: ChannelVisibility::Public,
            archived: false,
            is_default: name == "general",
            created_by: creator,
            participants: vec![],
            member_count: 3,
        }
    }

    fn people(&self, id: ConversationId, others: &[&User]) -> Conversation {
        let mut participants = vec![self.me.id];
        participants.extend(others.iter().map(|user| user.id));
        Conversation {
            id,
            workspace_id: self.workspace,
            kind: if others.len() == 1 {
                ConversationKind::Direct
            } else {
                ConversationKind::Group
            },
            name: None,
            topic: None,
            description: None,
            visibility: ChannelVisibility::Private,
            archived: false,
            is_default: false,
            created_by: Some(self.me.id),
            participants,
            member_count: 1 + others.len() as u32,
        }
    }

    fn bootstrap(
        &self,
        role: WorkspaceRole,
        user_state: Vec<ConversationUserState>,
    ) -> BootstrapResponse {
        BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: self.me.clone(),
            workspaces: vec![WorkspaceBootstrap {
                saved: Vec::new(),
                notification_settings: Default::default(),
                conversation_notification_prefs: Vec::new(),
                custom_emoji: Vec::new(),
                threads: Vec::new(),
                groups: Vec::new(),
                workspace: Workspace {
                    id: self.workspace,
                    name: "Lazy".into(),
                    settings: Default::default(),
                },
                role,
                conversations: vec![
                    self.channel(self.general, "general", None),
                    self.channel(self.design, "design", Some(self.me.id)),
                    self.people(self.dm_bob, &[&self.bob]),
                    self.people(self.group, &[&self.bob, &self.zoe]),
                ],
                user_state,
                members: vec![
                    self.me.clone(),
                    self.bob.clone(),
                    self.zoe.clone(),
                    self.carol.clone(),
                ],
                head: EventSeq::ZERO,
            }],
        }
    }

    fn quiet_state(&self) -> Vec<ConversationUserState> {
        [self.general, self.design, self.dm_bob, self.group]
            .into_iter()
            .map(|id| state(id, false, 0, 0, 0))
            .collect()
    }

    fn dispatch(&self, cx: &mut VisualTestContext, input: Input) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.run_until_parked();
    }

    fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.effects.0.borrow_mut())
    }

    /// Launches into `#general` (selected by the core) with the given history.
    fn reach(
        &self,
        cx: &mut VisualTestContext,
        bootstrap: BootstrapResponse,
        history: Vec<Message>,
    ) {
        self.dispatch(
            cx,
            Input::Started(StartupState {
                session_restored: true,
                ..Default::default()
            }),
        );
        self.dispatch(cx, Input::BootstrapLoaded(Box::new(bootstrap)));
        let generation = self
            .effects
            .0
            .borrow()
            .iter()
            .rev()
            .find_map(|effect| match effect {
                Effect::FetchHistory { generation, .. } => Some(*generation),
                _ => None,
            })
            .expect("the core fetches the selected conversation's history");
        self.dispatch(
            cx,
            Input::HistoryLoaded {
                conversation: self.general,
                generation,
                result: Ok(HistoryPage {
                    messages: history,
                    has_more: false,
                }),
            },
        );
        self.take_effects();
    }

    fn reach_quietly(&self, cx: &mut VisualTestContext) {
        self.reach(
            cx,
            self.bootstrap(WorkspaceRole::Member, self.quiet_state()),
            vec![],
        );
    }

    fn message(&self, author: &User, seq: i64) -> Message {
        Message {
            attachments: vec![],
            thread_root: None,
            in_channel: false,
            thread: None,
            mentions: Default::default(),
            id: MessageId::generate(),
            conversation_id: self.general,
            author_id: author.id,
            document: MessageDocument::from_plain_text(&format!("message {seq}")).unwrap(),
            created_at: Timestamp::now(),
            revision: 1,
            seq: EventSeq::new(seq).unwrap(),
            edited_at: None,
            reactions: vec![],
            reactions_as_of: EventSeq::ZERO,
            deleted: false,
        }
    }

    fn selected(&self, cx: &VisualTestContext) -> Option<ConversationId> {
        self.model
            .read_with(cx, |m, _| m.core().selected_conversation())
    }

    fn action(&self, cx: &VisualTestContext, kind: ActionKind) -> ActionState {
        self.model
            .read_with(cx, |m, _| m.core().action(kind).clone())
    }
}

fn label(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

fn refresh(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

fn on_screen(cx: &mut VisualTestContext, selector: &'static str) -> bool {
    refresh(cx);
    cx.debug_bounds(selector).is_some()
}

fn top(cx: &mut VisualTestContext, selector: &'static str) -> f32 {
    refresh(cx);
    f32::from(
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector:?} is not on screen"))
            .origin
            .y,
    )
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    refresh(cx);
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector:?} is not on screen"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

fn only<T>(items: Vec<T>) -> T {
    let mut items = items.into_iter();
    let first = items.next().expect("expected exactly one, found none");
    assert!(items.next().is_none(), "expected exactly one, found more");
    first
}

// ---- The sidebar ----

#[gpui::test]
fn the_sidebar_groups_conversations_and_shows_unread_and_mention_badges(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let user_state = vec![
        state(world.general, false, 0, 4, 0), // selected: no badge
        state(world.design, true, 0, 0, 0),   // a favorite
        state(world.dm_bob, false, 0, 2, 0),
        state(world.group, false, 0, 3, 1),
    ];
    world.reach(
        cx,
        world.bootstrap(WorkspaceRole::Member, user_state),
        vec![],
    );

    // Favorites first, then channels, then people.
    let (design, general, direct) = (
        top(cx, "conv-design"),
        top(cx, "conv-general"),
        top(cx, "conv-User bob"),
    );
    assert!(
        design < general,
        "the favorite is listed above the channels"
    );
    assert!(general < direct, "channels come before direct messages");

    assert!(
        on_screen(cx, "badge-User bob"),
        "an unread direct message shows its count"
    );
    assert!(
        on_screen(cx, label("mention-badge-User bob, User zoe".into())),
        "a mention is marked"
    );
    assert!(on_screen(cx, label("badge-User bob, User zoe".into())));
    assert!(
        !on_screen(cx, "badge-general"),
        "the open conversation shows no badge"
    );
    assert!(
        !on_screen(cx, "badge-design"),
        "nothing unread, nothing shown"
    );
}

#[gpui::test]
fn favoriting_moves_a_conversation_to_the_favorites_section(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_quietly(cx);
    assert!(
        top(cx, "conv-design") < top(cx, "conv-general"),
        "alphabetical to begin with"
    );

    click(cx, "favorite-toggle");
    let effects = world.take_effects();
    assert!(
        effects.iter().any(|e| matches!(e, Effect::SetFavorite { conversation, favorite: true } if *conversation == world.general)),
        "the star asks the core to favorite #general: {effects:?}"
    );
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::SetFavorite(world.general),
            result: Ok(ActionOutcome::UserState(state(
                world.general,
                true,
                0,
                0,
                0,
            ))),
        },
    );
    assert!(
        top(cx, "conv-general") < top(cx, "conv-design"),
        "#general now sits in Favorites"
    );
}

#[gpui::test]
fn an_empty_workspace_offers_browse_and_new_message(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let mut bootstrap = world.bootstrap(WorkspaceRole::Member, vec![]);
    bootstrap.workspaces[0].conversations.clear();
    world.dispatch(
        cx,
        Input::Started(StartupState {
            session_restored: true,
            ..Default::default()
        }),
    );
    world.dispatch(cx, Input::BootstrapLoaded(Box::new(bootstrap)));
    world.take_effects();

    assert!(
        on_screen(cx, "sidebar-browse-channels"),
        "the sidebar suggests browsing"
    );
    assert!(on_screen(cx, "sidebar-new-message"));
    assert!(
        on_screen(cx, "empty-browse"),
        "the main area explains and offers a next step"
    );
    click(cx, "empty-browse");
    assert!(
        on_screen(cx, "dialog-card"),
        "the button opens the browse dialog"
    );
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::LoadBrowse { .. }))
    );
}

// ---- Creating a channel ----

#[gpui::test]
fn creating_a_channel_entirely_from_the_keyboard_selects_it_and_focuses_the_composer(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach_quietly(cx);

    click(cx, "add-channel");
    assert!(on_screen(cx, "channels-menu"), "the + opens a menu");
    cx.simulate_keystrokes("enter"); // "Create a channel" is highlighted
    assert!(on_screen(cx, "dialog-card"));

    cx.simulate_input("Design Team");
    cx.simulate_keystrokes("enter");
    let request = match only(world.take_effects()) {
        Effect::CreateChannel { request, .. } => request,
        other => panic!("expected CreateChannel, got {other:?}"),
    };
    assert_eq!(request.name.as_str(), "design-team", "spaces become dashes");
    assert_eq!(request.visibility, ChannelVisibility::Public);
    assert!(matches!(
        world.action(cx, ActionKind::CreateChannel),
        ActionState::InFlight
    ));

    let created = world.channel(ConversationId::generate(), "design-team", Some(world.me.id));
    world.dispatch(
        cx,
        Input::ConversationCommandResult {
            operation: request.operation_id,
            result: Ok(ConversationResponse {
                conversation: created.clone(),
            }),
        },
    );
    assert_eq!(
        world.selected(cx),
        Some(created.id),
        "the new channel is open"
    );
    assert!(!on_screen(cx, "dialog-card"), "the dialog closed itself");
    cx.simulate_input("hello");
    let draft = world
        .model
        .read_with(cx, |m, _| m.core().draft(created.id).to_owned());
    assert_eq!(draft, "hello", "the caret is in the new channel's composer");
}

#[gpui::test]
fn the_visibility_choice_is_reachable_by_keyboard_and_sent(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_quietly(cx);
    click(cx, "add-channel");
    cx.simulate_keystrokes("enter");
    cx.simulate_input("secrets");
    // name -> topic -> visibility, then Right selects Private; on to Create.
    cx.simulate_keystrokes("tab tab right tab tab enter");
    let request = match only(world.take_effects()) {
        Effect::CreateChannel { request, .. } => request,
        other => panic!("expected CreateChannel, got {other:?}"),
    };
    assert_eq!(request.name.as_str(), "secrets");
    assert_eq!(request.visibility, ChannelVisibility::Private);
}

#[gpui::test]
fn a_taken_name_keeps_the_dialog_open_and_explains(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_quietly(cx);
    click(cx, "add-channel");
    cx.simulate_keystrokes("enter");
    cx.simulate_input("general");
    cx.simulate_keystrokes("enter");
    let operation = match only(world.take_effects()) {
        Effect::CreateChannel { request, .. } => request.operation_id,
        other => panic!("expected CreateChannel, got {other:?}"),
    };
    world.dispatch(
        cx,
        Input::ConversationCommandResult {
            operation,
            result: Err(RequestError::Rejected(ApiError::new(
                ErrorCode::NameTaken,
                "A channel named #general already exists.",
            ))),
        },
    );
    assert!(
        on_screen(cx, "dialog-card"),
        "the dialog stays so the name can be fixed"
    );
    assert!(
        matches!(world.action(cx, ActionKind::CreateChannel), ActionState::Failed(message) if message.contains("already taken"))
    );
}

#[gpui::test]
fn escape_closes_the_dialog_and_focus_returns_to_the_plus_button(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_quietly(cx);
    click(cx, "add-channel");
    cx.simulate_keystrokes("enter");
    assert!(on_screen(cx, "dialog-card"));
    cx.simulate_keystrokes("escape");
    assert!(!on_screen(cx, "dialog-card"), "Escape closes the dialog");
    cx.simulate_keystrokes("enter"); // the + has focus again, so Enter reopens its menu
    assert!(
        on_screen(cx, "channels-menu"),
        "focus was restored to the + button"
    );
}

/// The generation of the newest directory request among `effects`.
fn browse_generation(effects: &[Effect]) -> u64 {
    effects
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::LoadBrowse { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("the directory was asked for")
}

/// The server's answer to a directory request: one page, with nothing after it.
fn browse_page(generation: u64, channels: Vec<BrowsedChannel>) -> Input {
    Input::BrowseLoaded {
        generation,
        result: Ok(lazy_protocol::BrowseChannelsResponse {
            channels,
            has_more: false,
        }),
    }
}

// ---- Browsing and joining ----

#[gpui::test]
fn browsing_lists_public_channels_and_joining_opens_the_channel(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_quietly(cx);
    click(cx, "add-channel");
    cx.simulate_keystrokes("down enter"); // "Browse channels"
    assert!(on_screen(cx, "dialog-card"));
    let generation = browse_generation(&world.take_effects());

    let random = world.channel(ConversationId::generate(), "random", None);
    world.dispatch(
        cx,
        browse_page(
            generation,
            vec![
                BrowsedChannel {
                    conversation: world.channel(world.general, "general", None),
                    joined: true,
                },
                BrowsedChannel {
                    conversation: random.clone(),
                    joined: false,
                },
            ],
        ),
    );
    assert!(
        on_screen(cx, "browse-open-general"),
        "a joined channel offers Open"
    );
    assert!(
        on_screen(cx, "browse-join-random"),
        "an unjoined one offers Join"
    );

    click(cx, "browse-join-random");
    assert!(
        world.take_effects().iter().any(
            |e| matches!(e, Effect::JoinChannel { conversation } if *conversation == random.id)
        )
    );
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::Join(random.id),
            result: Ok(ActionOutcome::Conversation(random.clone())),
        },
    );
    assert_eq!(
        world.selected(cx),
        Some(random.id),
        "joining opens the channel"
    );
    assert!(!on_screen(cx, "dialog-card"), "and closes the dialog");
    assert!(
        on_screen(cx, "conv-random"),
        "the channel is in the sidebar now"
    );
}

#[gpui::test]
fn the_browse_filter_narrows_the_list_from_the_keyboard(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_quietly(cx);
    click(cx, "add-channel");
    cx.simulate_keystrokes("down enter");
    let generation = browse_generation(&world.take_effects());
    let recipes = BrowsedChannel {
        conversation: world.channel(ConversationId::generate(), "recipes", None),
        joined: false,
    };
    world.dispatch(
        cx,
        browse_page(
            generation,
            vec![
                BrowsedChannel {
                    conversation: world.channel(ConversationId::generate(), "random", None),
                    joined: false,
                },
                recipes.clone(),
                BrowsedChannel {
                    conversation: world.channel(ConversationId::generate(), "support", None),
                    joined: false,
                },
            ],
        ),
    );
    assert!(on_screen(cx, "browse-row-random") && on_screen(cx, "browse-row-support"));
    // The search is the server's: typing asks again with the text, and the answer is the list.
    cx.simulate_input("rec");
    let effects = world.take_effects();
    let asked = effects
        .iter()
        .rev()
        .find_map(|e| match e {
            Effect::LoadBrowse {
                generation, query, ..
            } => Some((*generation, query.query.clone())),
            _ => None,
        })
        .expect("typing searches the directory");
    assert_eq!(asked.1.as_deref(), Some("rec"));
    world.dispatch(cx, browse_page(asked.0, vec![recipes]));
    assert!(on_screen(cx, "browse-row-recipes"));
    assert!(
        !on_screen(cx, "browse-row-support"),
        "non-matching channels drop out"
    );
    cx.simulate_keystrokes("enter"); // joins the highlighted match
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::JoinChannel { .. })),
        "Enter joins the highlighted channel"
    );
}

// ---- Direct and group messages ----

#[gpui::test]
fn starting_a_direct_message_from_the_keyboard(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_quietly(cx);
    click(cx, "new-message");
    assert!(on_screen(cx, "dialog-card"));
    cx.simulate_input("car");
    cx.simulate_keystrokes("enter"); // adds Carol
    assert!(on_screen(
        cx,
        label(format!("picker-chip-{}", world.carol.id))
    ));
    cx.simulate_keystrokes("enter"); // empty search: confirm
    let request = match only(world.take_effects()) {
        Effect::OpenConversation { request, .. } => request,
        other => panic!("expected OpenConversation, got {other:?}"),
    };
    assert_eq!(request.participants, vec![world.carol.id]);

    let opened = world.people(ConversationId::generate(), &[&world.carol]);
    world.dispatch(
        cx,
        Input::ConversationCommandResult {
            operation: request.operation_id,
            result: Ok(ConversationResponse {
                conversation: opened.clone(),
            }),
        },
    );
    assert_eq!(world.selected(cx), Some(opened.id));
    assert!(!on_screen(cx, "dialog-card"));
    assert!(
        on_screen(cx, "conv-User carol"),
        "the direct message is in the sidebar, titled by the other person"
    );
}

#[gpui::test]
fn adding_people_to_a_direct_message_starts_a_new_conversation(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_quietly(cx);
    click(cx, "conv-User bob");
    assert_eq!(world.selected(cx), Some(world.dm_bob));
    click(cx, "details-button");
    assert!(on_screen(cx, "dialog-card"));
    click(cx, "details-add-people");

    // The people dialog opens prefilled with the people already in the conversation.
    assert!(
        on_screen(cx, label(format!("picker-chip-{}", world.bob.id))),
        "Bob is already chosen"
    );
    cx.simulate_input("zo");
    cx.simulate_keystrokes("enter");
    cx.simulate_keystrokes("enter");
    let request = match only(
        world
            .take_effects()
            .into_iter()
            .filter(|e| matches!(e, Effect::OpenConversation { .. }))
            .collect(),
    ) {
        Effect::OpenConversation { request, .. } => request,
        other => panic!("expected OpenConversation, got {other:?}"),
    };
    assert_eq!(
        request.participants,
        vec![world.bob.id, world.zoe.id],
        "a new conversation with the larger set"
    );
    assert_eq!(
        world.selected(cx),
        Some(world.dm_bob),
        "nothing was added to the old conversation"
    );
}

// ---- Managing a channel ----

#[gpui::test]
fn leaving_a_channel_asks_for_confirmation_first(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_quietly(cx);
    click(cx, "conv-design");
    click(cx, "details-button");
    world.take_effects(); // selecting #design fetched its history
    click(cx, "details-leave");
    assert!(on_screen(cx, "details-confirm-ok"), "leaving asks first");
    assert!(
        world.take_effects().is_empty(),
        "nothing is sent until it is confirmed"
    );

    click(cx, "details-confirm-ok");
    assert!(world.take_effects().iter().any(
        |e| matches!(e, Effect::LeaveChannel { conversation } if *conversation == world.design)
    ));
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::Leave(world.design),
            result: Ok(ActionOutcome::Done),
        },
    );
    assert!(
        !on_screen(cx, "dialog-card"),
        "the dialog closes once the channel is gone"
    );
    assert!(!on_screen(cx, "conv-design"), "and it leaves the sidebar");
    assert_eq!(
        world.selected(cx),
        Some(world.general),
        "the person lands in the default channel"
    );
}

#[gpui::test]
fn cancelling_a_confirmation_returns_to_the_details(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_quietly(cx);
    click(cx, "conv-design");
    click(cx, "details-button");
    world.take_effects();
    click(cx, "details-leave");
    cx.simulate_keystrokes("escape"); // Escape backs out of the confirmation, not the dialog
    assert!(on_screen(cx, "dialog-card"), "the dialog is still open");
    assert!(on_screen(cx, "details-leave"), "showing the details again");
    assert!(world.take_effects().is_empty());
}

#[gpui::test]
fn the_default_channel_cannot_be_left_or_archived(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(
        cx,
        world.bootstrap(WorkspaceRole::Owner, world.quiet_state()),
        vec![],
    );
    click(cx, "details-button"); // #general is selected
    assert!(on_screen(cx, "dialog-card"));
    assert!(
        !on_screen(cx, "details-leave"),
        "no Leave for the default channel"
    );
    assert!(
        !on_screen(cx, "details-archive"),
        "no Archive for it, even for an owner"
    );
}

#[gpui::test]
fn archiving_a_channel_makes_its_composer_read_only(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(
        cx,
        world.bootstrap(WorkspaceRole::Owner, world.quiet_state()),
        vec![],
    );
    click(cx, "conv-design");
    click(cx, "details-button");
    click(cx, "details-archive");
    click(cx, "details-confirm-ok");
    assert!(world.take_effects().iter().any(|e| matches!(e, Effect::ArchiveConversation { conversation } if *conversation == world.design)));

    let mut archived = world.channel(world.design, "design", Some(world.me.id));
    archived.archived = true;
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::Archive(world.design),
            result: Ok(ActionOutcome::Conversation(archived)),
        },
    );
    assert!(!on_screen(cx, "dialog-card"));
    cx.simulate_input("can I still type?");
    let draft = world
        .model
        .read_with(cx, |m, _| m.core().draft(world.design).to_owned());
    assert_eq!(draft, "", "an archived channel accepts no input");
}

#[gpui::test]
fn editing_the_topic_sends_only_what_changed(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_quietly(cx);
    click(cx, "conv-design");
    click(cx, "details-button");
    // The topic field has focus (the person may edit it); replace its text.
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("Fresh topic");
    click(cx, "details-save");
    let updates: Vec<_> = world
        .take_effects()
        .into_iter()
        .filter(|e| matches!(e, Effect::UpdateConversation { .. }))
        .collect();
    match only(updates) {
        Effect::UpdateConversation {
            conversation,
            request,
        } => {
            assert_eq!(conversation, world.design);
            assert_eq!(request.topic.as_deref(), Some("Fresh topic"));
            assert!(
                request.name.is_none() && request.description.is_none(),
                "only the topic changed"
            );
        }
        other => panic!("expected UpdateConversation, got {other:?}"),
    }
}

// ---- Read state ----

#[gpui::test]
fn opening_a_conversation_marks_it_read_and_shows_where_the_new_messages_begin(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    // Two read messages (positions 4 and 5), then three unread from Bob.
    let user_state = vec![
        state(world.general, false, 5, 3, 0),
        state(world.design, false, 0, 0, 0),
        state(world.dm_bob, false, 0, 0, 0),
        state(world.group, false, 0, 0, 0),
    ];
    let history = vec![
        world.message(&world.me, 4),
        world.message(&world.bob, 5),
        world.message(&world.bob, 6),
        world.message(&world.bob, 7),
        world.message(&world.bob, 8),
    ];
    world.reach(
        cx,
        world.bootstrap(WorkspaceRole::Member, user_state),
        history,
    );

    // The test platform's window is never active, and the UI told the core so.
    assert!(
        !world.model.read_with(cx, |m, _| m.core().window_active()),
        "the shell reports window activity"
    );
    assert!(
        !world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::MarkRead { .. })),
        "nothing is marked read while the window is inactive"
    );

    // Once the window is active and the newest message is on screen, the conversation is read.
    world.dispatch(cx, Input::WindowActive(true));
    refresh(cx);
    let marked = world.take_effects();
    assert!(
        marked.iter().any(|e| matches!(e, Effect::MarkRead { conversation, up_to } if *conversation == world.general && *up_to == EventSeq::new(8).unwrap())),
        "the timeline reported that the newest message is visible: {marked:?}"
    );
    assert!(
        on_screen(cx, "new-messages-divider"),
        "the divider shows where unread messages began"
    );
}

#[gpui::test]
fn being_removed_from_the_open_conversation_falls_back_and_says_so(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_quietly(cx);
    click(cx, "conv-design");
    assert_eq!(world.selected(cx), Some(world.design));
    world.take_effects();
    world.dispatch(
        cx,
        Input::Frame(ServerFrame::Event(EventEnvelope {
            workspace_id: world.workspace,
            seq: EventSeq::new(40).unwrap(),
            occurred_at: Timestamp::now(),
            event: Event::MemberRemoved {
                conversation_id: world.design,
                user_id: world.me.id,
            },
        })),
    );
    assert_eq!(
        world.selected(cx),
        Some(world.general),
        "the person is moved somewhere valid"
    );
    assert!(
        !on_screen(cx, "conv-design"),
        "the conversation is gone from the sidebar"
    );
    assert!(
        on_screen(cx, "notice-banner"),
        "and the person is told what happened"
    );
}

#[gpui::test]
fn the_dialog_backdrop_blocks_the_sidebar_behind_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_quietly(cx);
    click(cx, "new-message");
    assert!(on_screen(cx, "dialog-card"));
    // Clicking where a sidebar row sits must not switch conversations (it dismisses instead).
    let row = cx
        .debug_bounds("conv-design")
        .expect("the row is laid out behind the scrim");
    cx.simulate_click(row.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        world.selected(cx),
        Some(world.general),
        "the covered row was not activated"
    );
}
