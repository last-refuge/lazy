//! Mentions on GPUI's test platform: how a message that mentions someone is drawn, `@` autocomplete
//! in the composer and the editor, and the sidebar's `@N` badges. The real views and the real core
//! run; a recording effect sink stands in for the runtime.
//!
//! Every keyboard claim is made with real key events through GPUI's dispatch. The test platform
//! gives every character the same width and never reports a window as active, so these prove
//! behavior and wiring; real glyph layout and colors are checked in the offscreen renders. Nothing
//! here exercises a real input method (composition is simulated), a screen reader, or a display.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, EntityInputHandler as _, Modifiers, TestAppContext, VisualTestContext,
    WindowAppearance,
};
use lazy_client_core::{Delivery, Effect, Input, RequestError, StartupState};
use lazy_domain::mention::{MapDirectory, MentionTarget};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    GroupHandle, GroupId, MessageDocument, MessageId, Timestamp, UserId, Username, WorkspaceId,
    WorkspaceRole,
};
use lazy_protocol::{
    ApiError, BootstrapResponse, Conversation, ConversationMembersResponse, ConversationUserState,
    ErrorCode, Event, EventEnvelope, HistoryPage, Mentions, Message, PROTOCOL_VERSION, ServerFrame,
    User, UserGroup, Workspace, WorkspaceBootstrap,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::mention_source::MentionTable;
use lazy_ui::rich_text::MentionLabels as _;
use lazy_ui::root_view::RootView;
use lazy_ui::suggest::{Item, MentionKind};

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
    root: Entity<RootView>,
    me: User,
    bob: User,
    zoe: User,
    carol: User,
    workspace: WorkspaceId,
    /// The default channel, which everyone is in.
    general: ConversationId,
    /// A public channel with only `me` and `bob` in it.
    design: ConversationId,
    /// A group `me` and `bob` are in.
    team: UserGroup,
    /// A group `me` is not in.
    ops: UserGroup,
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
    let (root, cx) = cx.add_window_view({
        let model = model.clone();
        move |window, cx| RootView::new(model, window, cx)
    });
    let workspace = WorkspaceId::generate();
    let (me, bob, zoe, carol) = (
        person("alice"),
        person("bob"),
        person("zoe"),
        person("carol"),
    );
    let team = UserGroup {
        id: GroupId::generate(),
        workspace_id: workspace,
        handle: GroupHandle::parse("team").unwrap(),
        name: DisplayName::parse("Design Team").unwrap(),
        members: vec![me.id, bob.id],
    };
    let ops = UserGroup {
        id: GroupId::generate(),
        workspace_id: workspace,
        handle: GroupHandle::parse("ops").unwrap(),
        name: DisplayName::parse("Operations").unwrap(),
        members: vec![zoe.id],
    };
    let world = World {
        model,
        effects,
        root,
        me,
        bob,
        zoe,
        carol,
        workspace,
        general: ConversationId::generate(),
        design: ConversationId::generate(),
        team,
        ops,
    };
    (world, cx)
}

fn state(conversation: ConversationId, unread: u32, mentions: u32) -> ConversationUserState {
    ConversationUserState {
        conversation_id: conversation,
        favorite: false,
        last_read: EventSeq::ZERO,
        unread_count: unread,
        mention_count: mentions,
    }
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

fn label(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

fn clipboard(cx: &VisualTestContext) -> Option<String> {
    cx.read_from_clipboard().and_then(|item| item.text())
}

impl World {
    fn channel(&self, id: ConversationId, name: &str, members: u32) -> Conversation {
        Conversation {
            id,
            workspace_id: self.workspace,
            kind: ConversationKind::Channel,
            name: Some(ChannelName::parse(name).unwrap()),
            topic: None,
            description: None,
            visibility: ChannelVisibility::Public,
            archived: false,
            is_default: name == "general",
            created_by: None,
            participants: vec![],
            member_count: members,
        }
    }

    fn bootstrap(&self, role: WorkspaceRole, mention_count_in_design: u32) -> BootstrapResponse {
        BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: self.me.clone(),
            workspaces: vec![WorkspaceBootstrap {
                saved: Vec::new(),
                notification_settings: Default::default(),
                conversation_notification_prefs: Vec::new(),
                custom_emoji: Vec::new(),
                threads: Vec::new(),
                groups: vec![self.team.clone(), self.ops.clone()],
                workspace: Workspace {
                    id: self.workspace,
                    name: "Lazy".into(),
                    settings: Default::default(),
                },
                role,
                conversations: vec![
                    self.channel(self.general, "general", 4),
                    self.channel(self.design, "design", 2),
                ],
                user_state: vec![
                    state(self.general, 0, 0),
                    state(
                        self.design,
                        mention_count_in_design,
                        mention_count_in_design,
                    ),
                ],
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

    fn dispatch(&self, cx: &mut VisualTestContext, input: Input) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.run_until_parked();
    }

    fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.effects.0.borrow_mut())
    }

    /// Launches into `#general` as `role`, with `history` in it, and answers the member lists the
    /// core asks for the way the server would.
    fn reach_as(&self, cx: &mut VisualTestContext, role: WorkspaceRole, history: Vec<Message>) {
        self.dispatch(
            cx,
            Input::Started(StartupState {
                session_restored: true,
                ..Default::default()
            }),
        );
        self.dispatch(
            cx,
            Input::BootstrapLoaded(Box::new(self.bootstrap(role, 0))),
        );
        let effects = self.take_effects();
        let generation = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::FetchHistory { generation, .. } => Some(*generation),
                _ => None,
            })
            .expect("loading #general fetches its history");
        self.answer_member_fetches(cx, effects);
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
        let effects = self.take_effects();
        self.answer_member_fetches(cx, effects);
        draw(cx);
    }

    fn reach(&self, cx: &mut VisualTestContext, history: Vec<Message>) {
        self.reach_as(cx, WorkspaceRole::Member, history);
    }

    /// Answers the member lists the core asked for among `effects` the way the server would:
    /// everyone is in #general, and only the first two are in #design.
    fn answer_member_fetches(&self, cx: &mut VisualTestContext, effects: Vec<Effect>) {
        for effect in effects {
            let Effect::FetchMembers {
                conversation,
                generation,
            } = effect
            else {
                continue;
            };
            let people = if conversation == self.general {
                vec![self.me.id, self.bob.id, self.zoe.id, self.carol.id]
            } else {
                vec![self.me.id, self.bob.id]
            };
            self.dispatch(
                cx,
                Input::MembersFetched {
                    conversation,
                    generation,
                    result: Ok(ConversationMembersResponse {
                        conversation_id: conversation,
                        user_ids: people,
                    }),
                },
            );
        }
    }

    fn directory(&self) -> MapDirectory {
        let mut directory = MapDirectory::new();
        for user in [&self.me, &self.bob, &self.zoe, &self.carol] {
            directory.insert(user.username.as_str(), MentionTarget::User { id: user.id });
        }
        directory.insert("team", MentionTarget::Group { id: self.team.id });
        directory.insert("ops", MentionTarget::Group { id: self.ops.id });
        directory.insert("channel", MentionTarget::Channel);
        directory
    }

    /// A message in `conversation` from `author` whose text is `text`, notifying `notified`.
    fn message_in(
        &self,
        conversation: ConversationId,
        author: &User,
        text: &str,
        seq: i64,
        notified: Mentions,
    ) -> Message {
        Message {
            attachments: vec![],
            thread_root: None,
            in_channel: false,
            thread: None,
            mentions: notified,
            id: MessageId::generate(),
            conversation_id: conversation,
            author_id: author.id,
            document: MessageDocument::from_markdown_with(text, &self.directory()).unwrap(),
            created_at: Timestamp::now(),
            revision: 1,
            seq: EventSeq::new(seq).unwrap(),
            edited_at: None,
            reactions: vec![],
            reactions_as_of: EventSeq::ZERO,
            deleted: false,
        }
    }

    fn message(&self, author: &User, text: &str, seq: i64, notified: Mentions) -> Message {
        self.message_in(self.general, author, text, seq, notified)
    }

    fn notifying(users: &[&User]) -> Mentions {
        Mentions {
            users: users.iter().map(|user| user.id).collect(),
            channel: false,
        }
    }

    fn event(&self, cx: &mut VisualTestContext, seq: i64, event: Event) {
        self.dispatch(
            cx,
            Input::Frame(ServerFrame::Event(EventEnvelope {
                workspace_id: self.workspace,
                seq: EventSeq::new(seq).unwrap(),
                occurred_at: Timestamp::now(),
                event,
            })),
        );
    }

    fn on_screen(&self, cx: &mut VisualTestContext, selector: &'static str) -> bool {
        draw(cx);
        cx.debug_bounds(selector).is_some()
    }

    fn click(&self, cx: &mut VisualTestContext, selector: &'static str) {
        draw(cx);
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("nothing is labelled {selector:?} on screen"));
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();
    }

    fn hover_row(&self, cx: &mut VisualTestContext, index: usize) {
        draw(cx);
        let row = cx
            .debug_bounds(label(format!("message-row-{index}")))
            .unwrap_or_else(|| panic!("row {index} is not on screen"));
        cx.simulate_mouse_move(row.center(), None, Modifiers::default());
        cx.run_until_parked();
    }

    fn composer(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::composer::Composer> {
        self.root.read_with(cx, |root, cx| {
            root.shell().unwrap().read(cx).composer().clone()
        })
    }

    /// What the composer's suggestion list offers right now.
    fn offered(&self, cx: &mut VisualTestContext) -> Vec<Item> {
        draw(cx);
        let composer = self.composer(cx);
        composer.read_with(cx, |composer, _| composer.suggestion_items().to_vec())
    }

    /// The `@handle` of each person, group, or `@channel` on offer, in order.
    fn offered_handles(&self, cx: &mut VisualTestContext) -> Vec<String> {
        self.offered(cx)
            .into_iter()
            .filter_map(|item| match item {
                Item::Mention(choice) => Some(choice.handle),
                Item::Emoji(_) | Item::Custom(_) => None,
            })
            .collect()
    }

    fn draft(&self, cx: &VisualTestContext, conversation: ConversationId) -> String {
        self.model
            .read_with(cx, |m, _| m.core().draft(conversation).to_owned())
    }

    /// The messages the person has sent, as documents.
    fn sent(&self) -> Vec<MessageDocument> {
        self.take_effects()
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::PersistSubmission(entry) => Some(entry.document),
                _ => None,
            })
            .collect()
    }

    /// Opens a conversation from the sidebar and answers the member list the core asks for.
    fn select(&self, cx: &mut VisualTestContext, conversation_title: &'static str) {
        self.take_effects();
        self.click(cx, label(format!("conv-{conversation_title}")));
        let effects = self.take_effects();
        self.answer_member_fetches(cx, effects);
        draw(cx);
    }
}

fn mentioned(document: &MessageDocument) -> Vec<MentionTarget> {
    document.mentions()
}

// ---- How a message that mentions people is drawn ----------------------------------------------------

#[gpui::test]
fn a_message_that_mentions_me_is_tinted_and_one_that_mentions_someone_else_is_not(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let aimed_at_me = world.message(
        &world.bob,
        "hello @alice",
        1,
        World::notifying(&[&world.me]),
    );
    let aimed_at_zoe = world.message(&world.bob, "hello @zoe", 2, World::notifying(&[&world.zoe]));
    world.reach(cx, vec![aimed_at_me, aimed_at_zoe]);

    assert!(
        world.on_screen(cx, "mention-bar-0"),
        "the row that notifies me carries the bar"
    );
    assert!(
        !world.on_screen(cx, "mention-bar-1"),
        "a mention of someone else leaves the row plain"
    );
}

#[gpui::test]
fn my_own_message_is_never_tinted_even_when_it_names_me(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    // The server never notifies the author of their own message, but even if a stale copy says
    // so, the author is not "mentioned".
    let own = world.message(
        &world.me,
        "note to @alice",
        1,
        World::notifying(&[&world.me]),
    );
    world.reach(cx, vec![own]);
    assert!(!world.on_screen(cx, "mention-bar-0"));
}

#[gpui::test]
fn a_group_i_am_in_and_channel_mentions_tint_the_row_but_a_group_i_am_not_in_does_not(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let my_group = world.message(&world.bob, "ping @team", 1, World::notifying(&[&world.me]));
    let other_group = world.message(&world.bob, "ping @ops", 2, World::notifying(&[&world.zoe]));
    let everyone = world.message(
        &world.bob,
        "ping @channel",
        3,
        Mentions {
            users: vec![],
            channel: true,
        },
    );
    world.reach_as(
        cx,
        WorkspaceRole::Admin,
        vec![my_group, other_group, everyone],
    );

    assert!(world.on_screen(cx, "mention-bar-0"), "my group");
    assert!(!world.on_screen(cx, "mention-bar-1"), "a group I am not in");
    assert!(world.on_screen(cx, "mention-bar-2"), "@channel reaches me");
}

#[gpui::test]
fn pills_use_display_names_group_names_and_the_channel_and_fall_back_to_the_handle(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    let ghost = MentionTarget::User {
        id: UserId::generate(),
    };
    let document = MessageDocument::new(vec![lazy_domain::document::Block::Paragraph {
        inlines: vec![
            lazy_domain::document::Inline::mention(MentionTarget::User { id: world.bob.id }, "bob"),
            lazy_domain::document::Inline::mention(
                MentionTarget::User { id: world.me.id },
                "alice",
            ),
            lazy_domain::document::Inline::mention(
                MentionTarget::Group { id: world.team.id },
                "team",
            ),
            lazy_domain::document::Inline::mention(
                MentionTarget::Group { id: world.ops.id },
                "ops",
            ),
            lazy_domain::document::Inline::mention(MentionTarget::Channel, "channel"),
            lazy_domain::document::Inline::mention(ghost, "ghost"),
        ],
    }])
    .unwrap();

    let table = world.model.read_with(cx, |model, _| {
        MentionTable::for_document(model.core(), &document, false)
    });
    let look = |target: MentionTarget, handle: &str| table.look(target, handle);

    let bob = look(MentionTarget::User { id: world.bob.id }, "bob");
    assert_eq!(bob.text, "@User bob", "a person is shown by display name");
    assert!(!bob.aimed_at_me);
    let me = look(MentionTarget::User { id: world.me.id }, "alice");
    assert!(me.aimed_at_me, "a mention of me is emphasised");
    let team = look(MentionTarget::Group { id: world.team.id }, "team");
    assert_eq!(team.text, "@Design Team", "a group is shown by its name");
    assert!(team.aimed_at_me, "I am in the team");
    let ops = look(MentionTarget::Group { id: world.ops.id }, "ops");
    assert_eq!(ops.text, "@Operations");
    assert!(!ops.aimed_at_me, "I am not in ops");
    let channel = look(MentionTarget::Channel, "channel");
    assert_eq!(channel.text, "@channel");
    assert!(channel.aimed_at_me);
    let unknown = look(ghost, "ghost");
    assert_eq!(unknown.text, "@ghost", "an unknown person keeps the handle");
    assert!(!unknown.aimed_at_me);

    // On my own message nothing is aimed at me.
    let own = world.model.read_with(cx, |model, _| {
        MentionTable::for_document(model.core(), &document, true)
    });
    assert!(!own.look(MentionTarget::Channel, "channel").aimed_at_me);
    assert!(
        !own.look(MentionTarget::User { id: world.me.id }, "alice")
            .aimed_at_me
    );
}

#[gpui::test]
fn renaming_or_deleting_a_group_changes_how_old_pills_read(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    let document = MessageDocument::from_markdown_with("hi @team", &world.directory()).unwrap();
    let read = |cx: &VisualTestContext| {
        world.model.read_with(cx, |model, _| {
            MentionTable::for_document(model.core(), &document, false)
                .look(MentionTarget::Group { id: world.team.id }, "team")
                .text
        })
    };
    assert_eq!(read(cx), "@Design Team");

    let mut renamed = world.team.clone();
    renamed.name = DisplayName::parse("Designers").unwrap();
    world.event(cx, 1, Event::GroupChanged { group: renamed });
    assert_eq!(read(cx), "@Designers", "the pill follows the rename");

    world.event(
        cx,
        2,
        Event::GroupDeleted {
            group_id: world.team.id,
        },
    );
    assert_eq!(
        read(cx),
        "@team",
        "a deleted group falls back to the handle it was written with"
    );
}

// ---- Autocomplete in the composer -------------------------------------------------------------------

#[gpui::test]
fn typing_an_at_offers_people_and_enter_accepts_without_sending(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    cx.simulate_input("hey @bo");
    assert!(world.on_screen(cx, "composer-suggestions"));
    assert_eq!(world.offered_handles(cx), vec!["bob"]);

    world.take_effects();
    cx.simulate_keystrokes("enter");
    assert!(world.sent().is_empty(), "Enter accepted; it did not send");
    assert_eq!(world.draft(cx, world.general), "hey @bob ");
    assert!(!world.on_screen(cx, "composer-suggestions"));

    // With the list closed Enter sends, and the text became a real mention.
    cx.simulate_input("thanks");
    cx.simulate_keystrokes("enter");
    let sent = world.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        mentioned(&sent[0]),
        vec![MentionTarget::User { id: world.bob.id }]
    );
}

#[gpui::test]
fn the_list_shows_people_then_groups_and_keys_move_and_accept(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    cx.simulate_input("@");
    // Everyone in the channel (never me), then the groups by name; no @channel for a member of
    // the default channel.
    assert_eq!(
        world.offered_handles(cx),
        vec!["bob", "carol", "zoe", "team", "ops"]
    );
    let kinds: Vec<MentionKind> = world
        .offered(cx)
        .into_iter()
        .filter_map(|item| match item {
            Item::Mention(choice) => Some(choice.kind),
            Item::Emoji(_) | Item::Custom(_) => None,
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            MentionKind::User,
            MentionKind::User,
            MentionKind::User,
            MentionKind::Group,
            MentionKind::Group
        ]
    );

    // Down twice, then Up once: the second row. Enter takes it.
    cx.simulate_keystrokes("down down up");
    cx.simulate_keystrokes("enter");
    assert_eq!(world.draft(cx, world.general), "@carol ");

    // Tab accepts as well.
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input("@zo");
    cx.simulate_keystrokes("tab");
    assert_eq!(world.draft(cx, world.general), "@zoe ");
    assert!(!world.on_screen(cx, "composer-suggestions"));
}

#[gpui::test]
fn a_group_shows_how_many_people_are_in_it_and_becomes_a_group_mention(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    cx.simulate_input("cc @te");
    let offered = world.offered(cx);
    let [Item::Mention(choice)] = offered.as_slice() else {
        panic!("one group on offer, found {offered:?}")
    };
    assert_eq!(choice.kind, MentionKind::Group);
    assert_eq!(choice.name, "Design Team");
    assert_eq!(choice.members, Some(2));

    cx.simulate_keystrokes("enter");
    assert_eq!(world.draft(cx, world.general), "cc @team ");
    cx.simulate_keystrokes("enter");
    let sent = world.sent();
    assert_eq!(
        mentioned(&sent[0]),
        vec![MentionTarget::Group { id: world.team.id }]
    );
}

#[gpui::test]
fn in_a_public_channel_people_outside_it_are_offered_after_its_members_and_marked(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    world.select(cx, "design");
    world.take_effects();
    cx.simulate_input("@");
    // bob is in #design; carol and zoe are only in the workspace.
    let offered = world.offered(cx);
    let people: Vec<(String, bool)> = offered
        .iter()
        .filter_map(|item| match item {
            Item::Mention(choice) if choice.kind == MentionKind::User => {
                Some((choice.handle.clone(), choice.outside))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        people,
        vec![
            ("bob".to_owned(), false),
            ("carol".to_owned(), true),
            ("zoe".to_owned(), true)
        ]
    );

    // Choosing someone outside the channel still writes a mention.
    cx.simulate_input("zo");
    cx.simulate_keystrokes("enter");
    cx.simulate_keystrokes("enter");
    let sent = world.sent();
    assert_eq!(
        mentioned(&sent[0]),
        vec![MentionTarget::User { id: world.zoe.id }]
    );
}

#[gpui::test]
fn at_channel_is_offered_only_where_the_person_may_use_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    // A member in the default channel: not offered.
    world.reach(cx, vec![]);
    cx.simulate_input("@chan");
    assert!(
        !world.on_screen(cx, "composer-suggestions"),
        "members cannot use @channel in the default channel"
    );
    // A member in another channel: offered.
    world.select(cx, "design");
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input("@chan");
    assert_eq!(world.offered_handles(cx), vec!["channel"]);
}

#[gpui::test]
fn an_admin_is_offered_at_channel_in_the_default_channel(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Admin, vec![]);
    cx.simulate_input("@chan");
    let offered = world.offered(cx);
    let [Item::Mention(choice)] = offered.as_slice() else {
        panic!("only @channel matches, found {offered:?}")
    };
    assert_eq!(choice.kind, MentionKind::Channel);
    cx.simulate_keystrokes("enter");
    assert_eq!(world.draft(cx, world.general), "@channel ");
    cx.simulate_keystrokes("enter");
    let sent = world.sent();
    assert_eq!(mentioned(&sent[0]), vec![MentionTarget::Channel]);
}

#[gpui::test]
fn text_that_cannot_be_a_mention_stays_text_and_a_refusal_is_explained(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    world.take_effects();

    // A member typing @channel in the default channel: no list, and it sends as plain text.
    cx.simulate_input("@channel hello");
    cx.simulate_keystrokes("enter");
    let effects = world.take_effects();
    let entry = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry.clone()),
            _ => None,
        })
        .expect("the message was journaled");
    assert!(entry.document.mentions().is_empty(), "no mention was made");
    assert_eq!(entry.document.plain_text(), "@channel hello");

    // The server refuses it anyway; the person is told why, and can send again.
    world.dispatch(
        cx,
        Input::SubmissionPersisted {
            operation: entry.operation_id,
        },
    );
    world.dispatch(
        cx,
        Input::SendResult {
            operation: entry.operation_id,
            result: Err(RequestError::Rejected(ApiError::new(
                ErrorCode::Forbidden,
                "Only admins can mention everyone in the default channel.",
            ))),
        },
    );
    let delivery = world.model.read_with(cx, |m, _| {
        m.core()
            .timeline(world.general)
            .last()
            .unwrap()
            .delivery
            .clone()
    });
    let Delivery::Failed { reason, .. } = delivery else {
        panic!("the send failed, found {delivery:?}")
    };
    assert_eq!(
        reason,
        "Only admins can mention everyone in the default channel."
    );
    assert!(world.on_screen(cx, "retry-message"));
}

#[gpui::test]
fn nothing_is_offered_inside_code_after_a_word_or_after_a_backslash(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    for typed in [
        "run `@bo",
        "```\n@bo",
        "mail me at x@bo",
        "escaped \\@bo",
        "an email@bo",
    ] {
        cx.simulate_keystrokes("ctrl-a backspace");
        cx.simulate_input(typed);
        assert!(
            !world.on_screen(cx, "composer-suggestions"),
            "no list for {typed:?}"
        );
    }
    // And after the code is closed, a mention works again.
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input("`x` then @bo");
    assert_eq!(world.offered_handles(cx), vec!["bob"]);
}

#[gpui::test]
fn no_list_while_an_input_method_is_composing(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    let input = world
        .composer(cx)
        .read_with(cx, |composer, _| composer.input().clone());
    input.update_in(cx, |input, window, cx| {
        input.replace_and_mark_text_in_range(None, "@bo", None, window, cx)
    });
    assert!(input.read_with(cx, |input, _| input.is_composing()));
    assert!(
        !world.on_screen(cx, "composer-suggestions"),
        "preedit text is not final, so nothing is offered"
    );
}

#[gpui::test]
fn enter_sends_when_nothing_matches_or_the_list_was_dismissed(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    world.take_effects();

    cx.simulate_input("@zzz");
    assert!(!world.on_screen(cx, "composer-suggestions"));
    cx.simulate_keystrokes("enter");
    assert_eq!(world.sent().len(), 1, "no match, so Enter sends");

    cx.simulate_input("hi @bo");
    assert!(world.on_screen(cx, "composer-suggestions"));
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "composer-suggestions"));
    assert_eq!(
        world.draft(cx, world.general),
        "hi @bo",
        "Escape leaves the text alone"
    );
    cx.simulate_keystrokes("enter");
    let sent = world.sent();
    assert_eq!(sent.len(), 1, "after Escape, Enter sends the text");
    assert!(
        mentioned(&sent[0]).is_empty(),
        "@bo is not a handle, so it stays text"
    );
}

#[gpui::test]
fn the_list_stays_beside_the_caret_as_the_person_types(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    cx.simulate_input("@bo");
    draw(cx);
    draw(cx);
    let near_start = cx
        .debug_bounds("composer-suggestions")
        .expect("the list is open")
        .origin
        .x;

    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input("a long line of words before the mention @bo");
    draw(cx);
    draw(cx);
    let after_text = cx
        .debug_bounds("composer-suggestions")
        .expect("the list is open")
        .origin
        .x;
    assert!(
        after_text > near_start,
        "the list moves right with the caret ({near_start:?} then {after_text:?})"
    );
}

// ---- Autocomplete in the editor ---------------------------------------------------------------------

#[gpui::test]
fn the_editor_offers_mentions_and_saves_them_as_mentions(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let own = world.message(&world.me, "original", 1, Mentions::default());
    world.reach(cx, vec![own]);
    world.take_effects();
    world.hover_row(cx, 0);
    world.click(cx, "edit-message");
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("ping @bo");
    assert!(world.on_screen(cx, "editor-suggestions"));

    cx.simulate_keystrokes("enter");
    assert!(
        world.on_screen(cx, "message-editor"),
        "Enter accepted; the editor is still open"
    );
    cx.simulate_keystrokes("ctrl-a ctrl-c");
    assert_eq!(clipboard(cx).as_deref(), Some("ping @bob "));
    assert!(!world.on_screen(cx, "editor-suggestions"));

    // With the list closed Enter saves, and the saved text carries the mention.
    cx.simulate_keystrokes("enter");
    let saved = world.sent();
    assert_eq!(saved.len(), 1, "saving journals the edit");
    assert_eq!(
        mentioned(&saved[0]),
        vec![MentionTarget::User { id: world.bob.id }]
    );
}

// ---- Sidebar badges ---------------------------------------------------------------------------------

#[gpui::test]
fn the_sidebar_badge_counts_mentions_from_the_core_and_clears_when_the_conversation_opens(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    let count = |cx: &VisualTestContext| {
        world.model.read_with(cx, |m, _| {
            m.core()
                .user_state(world.design)
                .map_or(0, |s| s.mention_count)
        })
    };
    assert_eq!(count(cx), 0);
    assert!(!world.on_screen(cx, "mention-badge-design"));

    // Two messages in #design that notify me, and one that does not, arrive while I am in #general.
    let first = world.message_in(
        world.design,
        &world.bob,
        "look @alice",
        1,
        World::notifying(&[&world.me]),
    );
    let second = world.message_in(
        world.design,
        &world.bob,
        "also @alice",
        2,
        World::notifying(&[&world.me]),
    );
    let third = world.message_in(
        world.design,
        &world.bob,
        "unrelated",
        3,
        Mentions::default(),
    );
    let history = vec![first.clone(), second.clone(), third.clone()];
    for (seq, message) in [(1, first), (2, second), (3, third)] {
        world.event(
            cx,
            seq,
            Event::MessageCreated {
                message,
                operation_id: None,
            },
        );
    }
    assert_eq!(count(cx), 2, "only the messages that name me are counted");
    assert!(
        world.on_screen(cx, "mention-badge-design"),
        "the badge shows in the sidebar"
    );

    // Opening the conversation with the window active reads it: the core asks the server to mark
    // it read, and when that is answered the count and the badge go away.
    world.dispatch(cx, Input::WindowActive(true));
    world.take_effects();
    world.click(cx, "conv-design");
    let effects = world.take_effects();
    let generation = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::FetchHistory {
                conversation,
                generation,
                ..
            } if *conversation == world.design => Some(*generation),
            _ => None,
        })
        .expect("opening #design fetches its history");
    world.answer_member_fetches(cx, effects);
    world.dispatch(
        cx,
        Input::HistoryLoaded {
            conversation: world.design,
            generation,
            result: Ok(HistoryPage {
                messages: history,
                has_more: false,
            }),
        },
    );
    draw(cx);
    let up_to = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::MarkRead {
                conversation,
                up_to,
            } if conversation == world.design => Some(up_to),
            _ => None,
        })
        .expect("the newest message is on screen, so the conversation is marked read");
    assert_eq!(up_to, EventSeq::new(3).unwrap());
    world.dispatch(
        cx,
        Input::MarkReadResult {
            conversation: world.design,
            up_to,
            result: Ok(ConversationUserState {
                conversation_id: world.design,
                favorite: false,
                last_read: up_to,
                unread_count: 0,
                mention_count: 0,
            }),
        },
    );
    assert_eq!(count(cx), 0, "reading the conversation clears the count");
    assert!(!world.on_screen(cx, "mention-badge-design"));
}
