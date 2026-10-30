//! Custom emoji on GPUI's test platform (slice 3B): pictures inside messages, reaction chips, the
//! picker's Custom section, `:name` suggestions in the composer, the editor, and the thread's reply
//! box, and the admin dialog for adding, renaming, and deleting. The real views and the real core
//! run; a recording effect sink stands in for the runtime, and the tests answer its image fetches
//! and uploads the way the runtime would.
//!
//! Every keyboard claim is made with real key events through GPUI's dispatch, every mouse claim
//! with real mouse events. The test platform gives every character the same width, never reports
//! a window as active, and cannot show whether a picture was *painted*; those are decided by
//! reading the offscreen renders (`.dev/screens-3b`). Nothing here exercises a real input method
//! (composition is simulated), screen reader, file dialog, or display.

use std::cell::{Cell, RefCell};
use std::io::Cursor;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use gpui::{
    AppContext as _, Entity, EntityInputHandler as _, Modifiers, TestAppContext, VisualTestContext,
    WindowAppearance, point, px,
};
use lazy_client_core::{
    ActionKind, ActionOutcome, ActionState, Effect, ImageData, Input, StartupState, TransferError,
};
use lazy_domain::mention::{MapDirectory, MentionTarget};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EmojiId,
    EventSeq, MessageDocument, MessageId, ReactionEmoji, Timestamp, UserId, Username, WorkspaceId,
    WorkspaceRole,
};
use lazy_protocol::{
    ApiError, BootstrapResponse, Conversation, ConversationUserState, CustomEmoji,
    CustomEmojiResponse, ErrorCode, Event, EventEnvelope, HistoryPage, Message, PROTOCOL_VERSION,
    ReactionSummary, ServerFrame, ThreadSummary, User, Workspace, WorkspaceBootstrap,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::commands::{AppSnapshot, CommandId, availability};
use lazy_ui::image_store::{ImageKey, ImageStore, Slot};
use lazy_ui::root_view::RootView;
use lazy_ui::shell::ShellDialog;
use lazy_ui::suggest::Item;

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
    workspace: WorkspaceId,
    general: ConversationId,
    /// The workspace's emoji as the server lists them: two live, one deleted.
    emoji: Vec<CustomEmoji>,
    role: Cell<WorkspaceRole>,
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

/// A PNG of `width` by `height` pixels.
fn png(width: u32, height: u32) -> Vec<u8> {
    let picture = image::RgbaImage::from_pixel(width, height, image::Rgba([200, 40, 40, 255]));
    let mut out = Cursor::new(Vec::new());
    picture.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// A file on disk that the file dialog can be answered with.
fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lazy-ui-custom-emoji-{}", UserId::generate()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn start(cx: &mut TestAppContext) -> (World, &mut VisualTestContext) {
    start_with(cx, true)
}

fn start_with(cx: &mut TestAppContext, with_emoji: bool) -> (World, &mut VisualTestContext) {
    cx.update(|cx| lazy_ui::init(cx, WindowAppearance::Light));
    let effects = Rc::new(Recorder::default());
    let model = cx.new(|_| AppModel::new(effects.clone()));
    let (root, cx) = cx.add_window_view({
        let model = model.clone();
        move |window, cx| RootView::new(model, window, cx)
    });
    let me = person("alice");
    let bob = person("bob");
    let workspace = WorkspaceId::generate();
    let made = |name: &str, deleted: bool| CustomEmoji {
        id: EmojiId::generate(),
        workspace_id: workspace,
        name: name.to_owned(),
        created_by: bob.id,
        created_at: Timestamp::now(),
        deleted,
    };
    let emoji = if with_emoji {
        vec![
            made("old_logo", true),
            made("partyparrot", false),
            made("shipit", false),
        ]
    } else {
        Vec::new()
    };
    let world = World {
        model,
        effects,
        root,
        me,
        bob,
        zoe: person("zoe"),
        workspace,
        general: ConversationId::generate(),
        emoji,
        role: Cell::new(WorkspaceRole::Member),
    };
    (world, cx)
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

fn clipboard(cx: &VisualTestContext) -> Option<String> {
    cx.read_from_clipboard().and_then(|item| item.text())
}

impl World {
    fn id(&self, name: &str) -> EmojiId {
        self.emoji
            .iter()
            .find(|emoji| emoji.name == name)
            .unwrap_or_else(|| panic!("no emoji named {name}"))
            .id
    }

    fn custom(&self, name: &str) -> ReactionEmoji {
        ReactionEmoji::Custom(self.id(name))
    }

    fn directory(&self) -> MapDirectory {
        let mut directory = MapDirectory::new();
        for user in [&self.me, &self.bob, &self.zoe] {
            directory.insert(user.username.as_str(), MentionTarget::User { id: user.id });
        }
        for emoji in &self.emoji {
            directory.insert_emoji(&emoji.name, emoji.id);
        }
        directory
    }

    fn dispatch(&self, cx: &mut VisualTestContext, input: Input) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.run_until_parked();
    }

    fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.effects.0.borrow_mut())
    }

    /// The image fetches asked for so far, left in place to be answered.
    fn fetched(&self) -> Vec<EmojiId> {
        self.effects
            .0
            .borrow()
            .iter()
            .filter_map(|effect| match effect {
                Effect::FetchEmojiImage { emoji } => Some(*emoji),
                _ => None,
            })
            .collect()
    }

    /// Removes and returns the image fetches asked for so far, leaving the other effects.
    fn take_fetches(&self) -> Vec<EmojiId> {
        let mut fetches = Vec::new();
        self.effects.0.borrow_mut().retain(|effect| match effect {
            Effect::FetchEmojiImage { emoji } => {
                fetches.push(*emoji);
                false
            }
            _ => true,
        });
        fetches
    }

    /// Answers every image fetch asked for so far with a `width` by `height` picture. Returns
    /// which emoji were answered.
    fn answer_images(&self, cx: &mut VisualTestContext, width: u32, height: u32) -> Vec<EmojiId> {
        let fetches = self.take_fetches();
        for emoji in &fetches {
            self.dispatch(
                cx,
                Input::EmojiImageLoaded {
                    emoji: *emoji,
                    result: Ok(ImageData::new(png(width, height))),
                },
            );
        }
        draw(cx);
        fetches
    }

    fn conversation(&self) -> Conversation {
        Conversation {
            id: self.general,
            workspace_id: self.workspace,
            kind: ConversationKind::Channel,
            name: Some(ChannelName::parse("general").unwrap()),
            topic: None,
            description: None,
            visibility: ChannelVisibility::Public,
            archived: false,
            is_default: true,
            created_by: None,
            participants: vec![],
            member_count: 3,
        }
    }

    fn bootstrap(&self) -> BootstrapResponse {
        BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: self.me.clone(),
            workspaces: vec![WorkspaceBootstrap {
                saved: Vec::new(),
                notification_settings: Default::default(),
                conversation_notification_prefs: Vec::new(),
                custom_emoji: self.emoji.clone(),
                threads: Vec::new(),
                groups: Vec::new(),
                workspace: Workspace {
                    id: self.workspace,
                    name: "Lazy".into(),
                    settings: Default::default(),
                },
                role: self.role.get(),
                conversations: vec![self.conversation()],
                user_state: vec![ConversationUserState {
                    conversation_id: self.general,
                    favorite: false,
                    last_read: EventSeq::ZERO,
                    unread_count: 0,
                    mention_count: 0,
                }],
                members: vec![self.me.clone(), self.bob.clone(), self.zoe.clone()],
                head: EventSeq::ZERO,
            }],
        }
    }

    /// A message whose text is composer source: `:name:` becomes the workspace's emoji.
    fn message(&self, author: &User, text: &str, seq: i64) -> Message {
        let at = time::OffsetDateTime::from_unix_timestamp(1_790_000_000 + seq * 60).unwrap();
        Message {
            attachments: vec![],
            mentions: Default::default(),
            thread_root: None,
            in_channel: false,
            thread: None,
            id: MessageId::generate(),
            conversation_id: self.general,
            author_id: author.id,
            document: MessageDocument::from_markdown_with(text, &self.directory()).unwrap(),
            created_at: Timestamp::from(at),
            revision: 1,
            seq: EventSeq::new(seq).unwrap(),
            edited_at: None,
            reactions: vec![],
            reactions_as_of: EventSeq::ZERO,
            deleted: false,
        }
    }

    /// `message` with these reactions, each from the people given.
    fn reacted(&self, mut message: Message, spec: &[(ReactionEmoji, &[&User])]) -> Message {
        for (emoji, users) in spec {
            message.reactions.push(ReactionSummary {
                emoji: *emoji,
                users: users.iter().map(|user| user.id).collect(),
            });
        }
        message.reactions_as_of = EventSeq::new(message.seq.get() + 1).unwrap();
        message
    }

    fn reach(&self, cx: &mut VisualTestContext, messages: Vec<Message>) {
        self.reach_as(cx, WorkspaceRole::Member, messages);
    }

    fn reach_as(&self, cx: &mut VisualTestContext, role: WorkspaceRole, messages: Vec<Message>) {
        self.role.set(role);
        self.dispatch(
            cx,
            Input::Started(StartupState {
                session_restored: true,
                ..Default::default()
            }),
        );
        self.dispatch(cx, Input::BootstrapLoaded(Box::new(self.bootstrap())));
        let generation = self
            .take_effects()
            .into_iter()
            .find_map(|effect| match effect {
                Effect::FetchHistory { generation, .. } => Some(generation),
                _ => None,
            })
            .expect("loading #general fetches its history");
        self.dispatch(
            cx,
            Input::HistoryLoaded {
                conversation: self.general,
                generation,
                result: Ok(HistoryPage {
                    messages,
                    has_more: false,
                }),
            },
        );
        draw(cx);
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

    fn draft(&self, cx: &VisualTestContext) -> String {
        self.model
            .read_with(cx, |m, _| m.core().draft(self.general).to_owned())
    }

    fn recent(&self, cx: &VisualTestContext) -> Vec<ReactionEmoji> {
        self.model
            .read_with(cx, |m, _| m.core().emoji_prefs().recent.clone())
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

    fn picker_is_open(&self, cx: &mut VisualTestContext) -> bool {
        self.on_screen(cx, "emoji-picker")
    }

    fn open_composer_picker(&self, cx: &mut VisualTestContext) {
        self.click(cx, "composer-emoji");
        assert!(self.picker_is_open(cx), "the picker opens");
    }

    fn reaction_effects(&self) -> Vec<(ReactionEmoji, bool)> {
        self.take_effects()
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::SetReaction { request, .. } => Some((request.emoji, request.present)),
                _ => None,
            })
            .collect()
    }

    fn submitted(&self) -> Vec<lazy_client_core::JournalEntry> {
        self.take_effects()
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::PersistSubmission(entry) => Some(entry),
                _ => None,
            })
            .collect()
    }

    fn store(&self, cx: &mut VisualTestContext) -> (usize, u64) {
        cx.read(|app| {
            let store = ImageStore::global(app);
            let store = store.read(app);
            (store.decoded_count(), store.emoji_generation())
        })
    }

    fn slot(&self, cx: &mut VisualTestContext, id: EmojiId) -> Option<Slot> {
        cx.read(|app| {
            ImageStore::global(app)
                .read(app)
                .get(&ImageKey::Emoji(id))
                .cloned()
        })
    }

    fn timeline(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::timeline::TimelineView> {
        let shell = self
            .root
            .read_with(cx, |root, _| root.shell().unwrap().clone());
        shell.read_with(cx, |shell, _| shell.timeline().clone())
    }

    fn selected(&self, cx: &mut VisualTestContext) -> Option<String> {
        draw(cx);
        let timeline = self.timeline(cx);
        timeline.read_with(cx, |timeline, cx| timeline.selected_text(cx))
    }

    fn at(
        &self,
        cx: &mut VisualTestContext,
        row: usize,
        offset: usize,
    ) -> gpui::Point<gpui::Pixels> {
        draw(cx);
        let timeline = self.timeline(cx);
        timeline
            .read_with(cx, |timeline, cx| timeline.text_point(row, offset, cx))
            .unwrap_or_else(|| panic!("row {row} offset {offset} is not on screen"))
    }

    fn drag(
        &self,
        cx: &mut VisualTestContext,
        from: gpui::Point<gpui::Pixels>,
        to: gpui::Point<gpui::Pixels>,
    ) {
        let held = Some(gpui::MouseButton::Left);
        cx.simulate_mouse_down(from, gpui::MouseButton::Left, Modifiers::default());
        let middle = point((from.x + to.x) / 2.0, (from.y + to.y) / 2.0);
        cx.simulate_mouse_move(middle, held, Modifiers::default());
        cx.simulate_mouse_move(to, held, Modifiers::default());
        cx.simulate_mouse_up(to, gpui::MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
    }

    fn state(&self, cx: &VisualTestContext, kind: ActionKind) -> ActionState {
        self.model
            .read_with(cx, |m, _| m.core().action(kind).clone())
    }

    fn open_thread_of(&self, cx: &mut VisualTestContext, root: &Message, replies: Vec<Message>) {
        self.click(cx, "thread-line");
        let generation = self
            .effects
            .0
            .borrow()
            .iter()
            .rev()
            .find_map(|effect| match effect {
                Effect::FetchReplies {
                    root: r,
                    generation,
                    ..
                } if *r == root.id => Some(*generation),
                _ => None,
            })
            .expect("opening the thread asks for its replies");
        self.dispatch(
            cx,
            Input::RepliesLoaded {
                root: root.id,
                generation,
                result: Ok(HistoryPage {
                    messages: replies,
                    has_more: false,
                }),
            },
        );
        draw(cx);
    }
}

fn summary(replies: u32, replier: &User, as_of: i64) -> ThreadSummary {
    ThreadSummary {
        reply_count: replies,
        last_reply_seq: EventSeq::new(as_of).unwrap(),
        last_reply_at: Timestamp::now(),
        recent_repliers: vec![replier.id],
        as_of: EventSeq::new(as_of).unwrap(),
    }
}

// ---- Inside messages ----------------------------------------------------------------------------------

#[gpui::test]
fn a_message_asks_for_each_picture_once_and_decodes_it_once_however_often_it_is_used(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let first = world.message(
        &world.bob,
        "ship :shipit: it :shipit: now :shipit: and :partyparrot:",
        1,
    );
    let second = world.message(&world.zoe, "again :shipit:", 2);
    world.reach(cx, vec![first, second]);

    let mut asked = world.take_fetches();
    asked.sort();
    let mut expected = vec![world.id("shipit"), world.id("partyparrot")];
    expected.sort();
    assert_eq!(
        asked, expected,
        "one fetch for each picture, not for each use"
    );
    assert_eq!(
        world.store(cx).0,
        0,
        "nothing is decoded before the images arrive"
    );

    for emoji in &expected {
        world.dispatch(
            cx,
            Input::EmojiImageLoaded {
                emoji: *emoji,
                result: Ok(ImageData::new(png(64, 64))),
            },
        );
    }
    draw(cx);
    assert!(matches!(
        world.slot(cx, world.id("shipit")),
        Some(Slot::Ready(_))
    ));
    assert!(matches!(
        world.slot(cx, world.id("partyparrot")),
        Some(Slot::Ready(_))
    ));
    let (decoded, generation) = world.store(cx);
    assert_eq!(
        decoded, 2,
        "each picture was decoded once, on a background thread"
    );
    assert_eq!(
        generation, 2,
        "and the text was laid out again once for each"
    );

    // More frames change nothing: no new fetches, no new decodes.
    for _ in 0..5 {
        draw(cx);
    }
    assert_eq!(world.take_fetches(), vec![]);
    assert_eq!(world.store(cx), (2, 2));
}

#[gpui::test]
fn until_it_arrives_and_if_it_never_does_the_message_reads_with_the_name(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let message = world.message(&world.bob, "ship :shipit: now", 1);
    world.reach(cx, vec![message]);

    // Loading: the name is there to be selected and copied.
    let (from, to) = (
        world.at(cx, 0, 0),
        world.at(cx, 0, "ship :shipit: now".len()),
    );
    world.drag(cx, from, to);
    assert_eq!(world.selected(cx).as_deref(), Some("ship :shipit: now"));

    // The fetch fails: still the name, and nothing is asked for again or laid out again.
    world.take_fetches();
    world.dispatch(
        cx,
        Input::EmojiImageLoaded {
            emoji: world.id("shipit"),
            result: Err(TransferError::Unreadable("gone".into())),
        },
    );
    draw(cx);
    for _ in 0..3 {
        draw(cx);
    }
    assert_eq!(
        world.take_fetches(),
        vec![],
        "a failed picture is not asked for on every frame"
    );
    assert_eq!(
        world.store(cx),
        (0, 0),
        "nothing decoded, nothing re-measured"
    );
    let (from, to) = (
        world.at(cx, 0, 0),
        world.at(cx, 0, "ship :shipit: now".len()),
    );
    world.drag(cx, from, to);
    assert_eq!(world.selected(cx).as_deref(), Some("ship :shipit: now"));
}

#[gpui::test]
fn copying_a_message_with_pictures_writes_their_names(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let text = "go :shipit::partyparrot: and :shipit:!";
    world.reach(cx, vec![world.message(&world.bob, text, 1)]);
    world.answer_images(cx, 64, 64);
    assert_eq!(world.store(cx).0, 2);

    // A drag over all of it.
    let (from, to) = (world.at(cx, 0, 0), world.at(cx, 0, text.len()));
    world.drag(cx, from, to);
    assert_eq!(world.selected(cx).as_deref(), Some(text));

    // The message's own Copy text.
    cx.simulate_keystrokes("escape");
    cx.simulate_keystrokes("shift-tab enter");
    world.click(cx, "message-actions-copy-text");
    assert_eq!(clipboard(cx).as_deref(), Some(text));
}

#[gpui::test]
fn a_selection_never_splits_a_picture_from_its_name(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let text = "aa :shipit: bb";
    world.reach(cx, vec![world.message(&world.bob, text, 1)]);
    world.answer_images(cx, 64, 64);

    // The picture spans these two points (the edges of its name in the copied text).
    let (left, right) = (world.at(cx, 0, 3), world.at(cx, 0, 11));
    let end = world.at(cx, 0, text.len());
    let start = world.at(cx, 0, 0);
    let along = |fraction: f32| point(left.x + (right.x - left.x) * fraction, left.y);

    // Pressing on the near half of the picture and dragging to the end takes all of it...
    world.drag(cx, along(0.25), end);
    assert_eq!(world.selected(cx).as_deref(), Some(":shipit: bb"));
    cx.simulate_keystrokes("escape");
    // ...and on the far half, none of it: never a piece of the name.
    world.drag(cx, along(0.75), end);
    assert_eq!(world.selected(cx).as_deref(), Some(" bb"));
    cx.simulate_keystrokes("escape");
    // The same going the other way.
    world.drag(cx, start, along(0.75));
    assert_eq!(world.selected(cx).as_deref(), Some("aa :shipit:"));
    cx.simulate_keystrokes("escape");
    world.drag(cx, start, along(0.25));
    assert_eq!(world.selected(cx).as_deref(), Some("aa "));
}

#[gpui::test]
fn a_deleted_or_unknown_emoji_stays_its_name_and_is_never_fetched(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let ghost = EmojiId::generate();
    let mut message = world.message(&world.bob, "old :old_logo: here", 1);
    // One the workspace has never heard of (another workspace's, or from the future).
    let mut blocks = message.document.blocks().to_vec();
    let lazy_domain::Block::Paragraph { inlines } = &mut blocks[0] else {
        panic!("a paragraph");
    };
    inlines.push(lazy_domain::Inline::emoji(ghost, "ghost"));
    message.document = MessageDocument::new(blocks).unwrap();
    world.reach(cx, vec![message]);

    assert_eq!(world.take_fetches(), vec![], "neither is asked for");
    let text = "old :old_logo: here:ghost:";
    let (from, to) = (world.at(cx, 0, 0), world.at(cx, 0, text.len()));
    world.drag(cx, from, to);
    assert_eq!(world.selected(cx).as_deref(), Some(text));
    assert_eq!(world.store(cx), (0, 0));
}

#[gpui::test]
fn a_rename_shows_in_messages_at_once_and_a_deletion_turns_the_picture_back_into_a_name(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![world.message(&world.bob, "ship :shipit: now", 1)]);
    world.answer_images(cx, 64, 64);

    let renamed = CustomEmoji {
        name: "launch".into(),
        ..world
            .emoji
            .iter()
            .find(|e| e.name == "shipit")
            .unwrap()
            .clone()
    };
    world.event(
        cx,
        2,
        Event::EmojiChanged {
            emoji: renamed.clone(),
        },
    );
    let (from, to) = (
        world.at(cx, 0, 0),
        world.at(cx, 0, "ship :launch: now".len()),
    );
    world.drag(cx, from, to);
    assert_eq!(
        world.selected(cx).as_deref(),
        Some("ship :launch: now"),
        "the new name, though the message was written with the old one"
    );

    let deleted = CustomEmoji {
        deleted: true,
        ..renamed
    };
    world.event(cx, 3, Event::EmojiChanged { emoji: deleted });
    cx.simulate_keystrokes("escape");
    let (from, to) = (
        world.at(cx, 0, 0),
        world.at(cx, 0, "ship :launch: now".len()),
    );
    world.drag(cx, from, to);
    assert_eq!(world.selected(cx).as_deref(), Some("ship :launch: now"));
}

#[gpui::test]
fn the_same_picture_in_a_thread_reply_and_the_channel_is_decoded_once(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let root = Message {
        thread: Some(summary(1, &world.zoe, 3)),
        ..world.message(&world.bob, "root :shipit:", 1)
    };
    let reply = Message {
        thread_root: Some(root.id),
        ..world.message(&world.zoe, "reply :shipit:", 3)
    };
    world.reach(cx, vec![root.clone()]);
    world.answer_images(cx, 64, 64);
    assert_eq!(world.store(cx).0, 1);
    world.open_thread_of(cx, &root, vec![reply]);
    assert!(world.on_screen(cx, "thread-pane"));
    assert_eq!(
        world.take_fetches(),
        vec![],
        "the pane reuses what the channel fetched"
    );
    assert_eq!(world.store(cx).0, 1, "and what it decoded");
}

// ---- Reaction chips -----------------------------------------------------------------------------------

fn chat_with_custom_reactions(world: &World, cx: &mut VisualTestContext) {
    let reacted = world.reacted(
        world.message(&world.bob, "ship it", 1),
        &[
            (world.custom("shipit"), &[&world.bob, &world.zoe]),
            (ReactionEmoji::parse("👍").unwrap(), &[&world.me]),
            (world.custom("old_logo"), &[&world.me, &world.zoe]),
        ],
    );
    world.reach(cx, vec![reacted]);
}

#[gpui::test]
fn a_custom_reaction_is_its_name_until_the_picture_arrives_then_the_picture_and_a_count(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    chat_with_custom_reactions(&world, cx);
    assert!(world.on_screen(cx, "reaction-chip-0"));
    assert!(
        world.on_screen(cx, "reaction-emoji-fallback"),
        "its name stands in while the picture loads"
    );
    assert!(!world.on_screen(cx, "reaction-emoji-picture"));
    assert_eq!(
        world.take_fetches(),
        vec![world.id("shipit")],
        "the live emoji is fetched; the deleted one is not"
    );

    world.dispatch(
        cx,
        Input::EmojiImageLoaded {
            emoji: world.id("shipit"),
            result: Ok(ImageData::new(png(64, 64))),
        },
    );
    draw(cx);
    assert!(world.on_screen(cx, "reaction-emoji-picture"));
}

#[gpui::test]
fn the_chip_says_who_reacted_with_the_name(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    chat_with_custom_reactions(&world, cx);
    let (tooltips, labels) = world.model.read_with(cx, |model, app| {
        let core = model.core();
        let item = core.timeline(world.general)[0].clone();
        let store = ImageStore::global(app);
        let chips = lazy_ui::reactions::chips_for(core, store.read(app), &item);
        (
            chips
                .iter()
                .map(|c| c.tooltip().to_string())
                .collect::<Vec<_>>(),
            chips
                .iter()
                .map(|c| c.label().to_string())
                .collect::<Vec<_>>(),
        )
    });
    assert_eq!(tooltips[0], "User bob and User zoe reacted with :shipit:");
    assert!(labels[0].contains(":shipit:"), "{labels:?}");
    assert_eq!(
        tooltips[2], "You and User zoe reacted with :old_logo:",
        "a deleted emoji reads as its name"
    );
}

#[gpui::test]
fn clicking_a_custom_chip_adds_the_persons_own_reaction(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    chat_with_custom_reactions(&world, cx);
    world.take_effects();
    world.click(cx, "reaction-chip-0");
    assert_eq!(
        world.reaction_effects(),
        vec![(world.custom("shipit"), true)]
    );
    assert!(
        world
            .model
            .read_with(cx, |m, _| m.core().timeline(world.general)[0]
                .reacted_by(world.me.id, world.custom("shipit"))),
        "shown at once"
    );
}

#[gpui::test]
fn clicking_a_custom_chip_that_is_the_persons_own_withdraws_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let mine = world.reacted(
        world.message(&world.bob, "ship it", 1),
        &[(world.custom("shipit"), &[&world.me, &world.bob])],
    );
    world.reach(cx, vec![mine]);
    world.take_effects();
    world.click(cx, "reaction-chip-0");
    assert_eq!(
        world.reaction_effects(),
        vec![(world.custom("shipit"), false)]
    );
}

#[gpui::test]
fn a_deleted_emoji_can_be_withdrawn_but_not_added(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    // Zoe and Alice used :old_logo: before it was deleted.
    let reacted = world.reacted(
        world.message(&world.bob, "old news", 1),
        &[(world.custom("old_logo"), &[&world.me, &world.zoe])],
    );
    world.reach(cx, vec![reacted]);
    assert!(
        world.on_screen(cx, "reaction-emoji-fallback"),
        "shown as :old_logo:"
    );
    world.take_effects();

    // Mine: clicking withdraws it.
    world.click(cx, "reaction-chip-0");
    assert_eq!(
        world.reaction_effects(),
        vec![(world.custom("old_logo"), false)]
    );
}

#[gpui::test]
fn a_deleted_emoji_someone_else_used_cannot_be_added_by_clicking(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let only_zoe = world.reacted(
        world.message(&world.bob, "older news", 1),
        &[(world.custom("old_logo"), &[&world.zoe])],
    );
    world.reach(cx, vec![only_zoe]);
    world.take_effects();
    world.click(cx, "reaction-chip-0");
    assert_eq!(world.reaction_effects(), vec![], "nothing is sent");
    assert!(
        !world
            .model
            .read_with(cx, |m, _| m.core().timeline(world.general)[0]
                .reacted_by(world.me.id, world.custom("old_logo"))),
        "and it did not appear as the person's"
    );
}

// ---- The picker ---------------------------------------------------------------------------------------

#[gpui::test]
fn the_custom_section_comes_first_and_its_pictures_are_fetched_when_shown(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    world.open_composer_picker(cx);
    draw(cx);
    let mut asked = world.fetched();
    asked.sort();
    let mut live = vec![world.id("partyparrot"), world.id("shipit")];
    live.sort();
    assert_eq!(asked, live, "the live emoji, not the deleted one");
    world.answer_images(cx, 64, 64);
    assert!(world.on_screen(cx, "emoji-custom-picture"));
    assert!(
        !world.on_screen(cx, "emoji-custom-empty"),
        "there is something in it"
    );
    assert!(world.on_screen(cx, "emoji-cell-0") && world.on_screen(cx, "emoji-cell-1"));

    // Down from the search field enters the grid on the first custom emoji; Enter types its name.
    cx.simulate_keystrokes("down enter");
    assert!(!world.picker_is_open(cx));
    assert_eq!(world.draft(cx), ":partyparrot:");
    assert_eq!(world.recent(cx), vec![world.custom("partyparrot")]);

    // Focus is back in the composer.
    cx.simulate_input(" ok");
    assert_eq!(world.draft(cx), ":partyparrot: ok");
}

#[gpui::test]
fn clicking_a_custom_emoji_in_the_composer_picker_types_its_name(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    world.open_composer_picker(cx);
    world.answer_images(cx, 64, 64);
    world.click(cx, "emoji-cell-1");
    assert_eq!(world.draft(cx), ":shipit:");
}

#[gpui::test]
fn the_group_tab_for_custom_comes_right_after_recently_used(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    // Use a standard emoji first so that Recently used exists.
    world.open_composer_picker(cx);
    cx.simulate_input("tada");
    cx.simulate_keystrokes("enter");
    world.open_composer_picker(cx);
    assert!(world.on_screen(cx, "emoji-tab-0"), "Recently used");
    assert!(world.on_screen(cx, "emoji-tab-1"), "Custom");
    // Tabs: Right from the first tab goes to Custom, and Down enters the grid at its first emoji
    // (Recently used has one item: 🎉, then Custom's two).
    cx.simulate_keystrokes("tab right down enter");
    assert_eq!(world.draft(cx), "🎉:partyparrot:");
}

#[gpui::test]
fn searching_finds_custom_emoji_by_name_and_lists_them_first(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    world.open_composer_picker(cx);
    cx.simulate_input("ship");
    world.answer_images(cx, 64, 64);
    assert!(
        world.on_screen(cx, "emoji-custom-picture"),
        "the custom emoji is a result, with its picture"
    );
    assert!(
        world.on_screen(cx, "emoji-preview-name"),
        "and the preview line names it"
    );
    cx.simulate_keystrokes("enter");
    assert_eq!(world.draft(cx), ":shipit:", "before the standard :ship: 🚢");
}

#[gpui::test]
fn a_deleted_emoji_is_not_in_the_picker_or_search(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    world.open_composer_picker(cx);
    cx.simulate_input("old_logo");
    assert!(
        !world.on_screen(cx, "emoji-custom-picture"),
        "nothing found for a deleted name"
    );
    cx.simulate_keystrokes("enter");
    assert_eq!(world.draft(cx), "", "and Enter has nothing to choose");
}

#[gpui::test]
fn choosing_a_custom_emoji_for_a_reaction_reacts_with_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![world.message(&world.bob, "ship it", 1)]);
    world.hover_and_react(cx);
    cx.simulate_input("ship");
    cx.simulate_keystrokes("enter");
    assert_eq!(
        world.reaction_effects(),
        vec![(world.custom("shipit"), true)]
    );
    assert_eq!(world.recent(cx), vec![world.custom("shipit")]);
    assert_eq!(world.draft(cx), "", "the composer was not touched");
}

impl World {
    fn hover_and_react(&self, cx: &mut VisualTestContext) {
        draw(cx);
        let row = cx
            .debug_bounds("message-row-0")
            .expect("row 0 is on screen");
        cx.simulate_mouse_move(row.center(), None, Modifiers::default());
        cx.run_until_parked();
        self.click(cx, "react-message");
        assert!(self.picker_is_open(cx));
    }
}

#[gpui::test]
fn a_deleted_emoji_leaves_recently_used(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    world.dispatch(cx, Input::RecordEmojiUse(world.custom("shipit")));
    world.dispatch(
        cx,
        Input::RecordEmojiUse(ReactionEmoji::parse("🎉").unwrap()),
    );
    assert_eq!(world.recent(cx).len(), 2);
    let gone = CustomEmoji {
        deleted: true,
        ..world
            .emoji
            .iter()
            .find(|e| e.name == "shipit")
            .unwrap()
            .clone()
    };
    world.event(cx, 2, Event::EmojiChanged { emoji: gone });
    assert_eq!(
        world.recent(cx),
        vec![ReactionEmoji::parse("🎉").unwrap()],
        "it can no longer be used, so it is not offered again"
    );
}

#[gpui::test]
fn with_no_custom_emoji_a_member_is_told_and_cannot_add_one(cx: &mut TestAppContext) {
    let (world, cx) = start_with(cx, false);
    world.reach(cx, vec![]);
    world.open_composer_picker(cx);
    assert!(
        world.on_screen(cx, "emoji-custom-empty"),
        "the section says so"
    );
    assert!(
        !world.on_screen(cx, "emoji-add-custom"),
        "and offers nothing to a member"
    );
    // The empty section is no stop for the keyboard: Down goes to the first standard emoji.
    cx.simulate_keystrokes("down enter");
    assert_eq!(
        world.draft(cx),
        lazy_domain::emoji::in_group(lazy_domain::emoji::groups().next().unwrap())
            .next()
            .unwrap()
            .as_str()
    );
}

#[gpui::test]
fn with_no_custom_emoji_an_admin_can_go_straight_to_adding_one(cx: &mut TestAppContext) {
    let (world, cx) = start_with(cx, false);
    world.reach_as(cx, WorkspaceRole::Admin, vec![]);
    world.open_composer_picker(cx);
    assert!(world.on_screen(cx, "emoji-custom-empty"));
    world.click(cx, "emoji-add-custom");
    assert!(
        !world.picker_is_open(cx),
        "the picker gives way to the dialog"
    );
    assert!(
        world.on_screen(cx, "emoji-admin-close"),
        "the custom emoji dialog is open"
    );
    assert!(world.on_screen(cx, "emoji-admin-empty"));
}

// ---- Suggestions after a colon ------------------------------------------------------------------------

#[gpui::test]
fn colon_suggestions_offer_custom_emoji_first_with_their_pictures(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    cx.simulate_input("nice :shi");
    assert!(world.on_screen(cx, "composer-suggestions-0"));
    let items = suggestions(&world, cx);
    assert!(
        matches!(&items[0], Item::Custom(choice) if choice.name == "shipit"),
        "custom first: {items:?}"
    );
    assert!(
        items.len() > 1 && matches!(items[1], Item::Emoji(_)),
        "then the standard ones (:shirt:, :shield:...): {items:?}"
    );
    assert_eq!(
        world.fetched(),
        vec![world.id("shipit")],
        "its picture is asked for"
    );
    assert!(world.on_screen(cx, "suggestion-emoji-placeholder"));
    world.answer_images(cx, 64, 64);
    assert!(
        world.on_screen(cx, "suggestion-emoji-picture"),
        "and shown once it is ready"
    );
}

fn suggestions(world: &World, cx: &mut VisualTestContext) -> Vec<Item> {
    let shell = world
        .root
        .read_with(cx, |root, _| root.shell().unwrap().clone());
    shell.read_with(cx, |shell, cx| {
        shell.composer().read(cx).suggestion_items().to_vec()
    })
}

#[gpui::test]
fn accepting_a_custom_suggestion_types_its_name_and_remembers_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    cx.simulate_input("nice :shi");
    world.take_effects();
    cx.simulate_keystrokes("enter");
    assert!(
        world.submitted().is_empty(),
        "Enter accepted; it did not send"
    );
    assert_eq!(world.draft(cx), "nice :shipit:");
    assert!(!world.on_screen(cx, "composer-suggestions"));
    assert_eq!(world.recent(cx), vec![world.custom("shipit")]);

    // Sending turns the name into the emoji.
    cx.simulate_keystrokes("enter");
    let sent = world.submitted();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].document.custom_emoji(), vec![world.id("shipit")]);
}

#[gpui::test]
fn a_click_on_a_custom_suggestion_accepts_it_and_tab_does_too(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    cx.simulate_input(":party");
    world.click(cx, "composer-suggestions-0");
    assert_eq!(world.draft(cx), ":partyparrot:");
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input(":party");
    cx.simulate_keystrokes("tab");
    assert_eq!(world.draft(cx), ":partyparrot:");
}

#[gpui::test]
fn colon_suggestions_keep_the_old_guards_and_skip_deleted_emoji(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    cx.simulate_input(":old_l");
    assert!(
        !world.on_screen(cx, "composer-suggestions"),
        "a deleted emoji is not offered"
    );
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input("at 10:shi");
    assert!(
        !world.on_screen(cx, "composer-suggestions"),
        "a colon inside a word asks for nothing"
    );
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input("`:shi");
    assert!(
        !world.on_screen(cx, "composer-suggestions"),
        "nor does code"
    );
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input(":s");
    assert!(
        !world.on_screen(cx, "composer-suggestions"),
        "one letter is too few"
    );
}

#[gpui::test]
fn the_editor_and_the_thread_reply_box_offer_custom_emoji_too(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let root = Message {
        thread: Some(summary(1, &world.zoe, 3)),
        ..world.message(&world.me, "root", 1)
    };
    let reply = Message {
        thread_root: Some(root.id),
        ..world.message(&world.zoe, "reply", 3)
    };
    world.reach(cx, vec![root.clone()]);
    world.take_effects();

    // The editor.
    draw(cx);
    let row = cx.debug_bounds("message-row-0").unwrap();
    cx.simulate_mouse_move(row.center(), None, Modifiers::default());
    cx.run_until_parked();
    world.click(cx, "edit-message");
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("well done :shi");
    assert!(world.on_screen(cx, "editor-suggestions"));
    // Its picture is fetched and, once it has arrived, shown.
    assert!(world.on_screen(cx, "suggestion-emoji-placeholder"));
    assert!(
        world
            .answer_images(cx, 64, 64)
            .contains(&world.id("shipit"))
    );
    assert!(world.on_screen(cx, "suggestion-emoji-picture"));
    cx.simulate_keystrokes("tab");
    cx.simulate_keystrokes("ctrl-a ctrl-c");
    assert_eq!(clipboard(cx).as_deref(), Some("well done :shipit:"));
    assert_eq!(world.recent(cx), vec![world.custom("shipit")]);
    cx.simulate_keystrokes("escape");
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "message-editor"));

    // The thread's reply box.
    world.open_thread_of(cx, &root, vec![reply]);
    assert!(world.on_screen(cx, "thread-pane"));
    world.click(cx, "thread-send-reply");
    world.take_effects();
    let input = world.root.read_with(cx, |root, app| {
        root.shell()
            .unwrap()
            .read(app)
            .thread_pane()
            .read(app)
            .composer()
            .read(app)
            .input()
            .clone()
    });
    input.update_in(cx, |input, window, cx| input.focus(window, cx));
    cx.simulate_input("thanks :party");
    assert!(world.on_screen(cx, "thread-composer-suggestions"));
    cx.simulate_keystrokes("enter");
    assert!(!world.on_screen(cx, "thread-composer-suggestions"));
    let text = input.read_with(cx, |input, _| input.text().to_owned());
    assert_eq!(text, "thanks :partyparrot:");
}

// ---- The admin dialog ---------------------------------------------------------------------------------

fn admin(cx: &mut TestAppContext) -> (World, &mut VisualTestContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Admin, vec![]);
    (world, cx)
}

impl World {
    /// Opens the dialog the way a person does: Menu, Workspace, Custom emoji.
    fn open_dialog(&self, cx: &mut VisualTestContext) {
        self.click(cx, "sidebar-menu");
        self.click(cx, "command-menu-tab-Workspace");
        self.click(cx, "command-menu-CustomEmoji");
        assert!(
            self.on_screen(cx, "emoji-admin-close"),
            "the dialog is open"
        );
    }

    fn uploads(&self) -> Vec<(EmojiId, String)> {
        self.effects
            .0
            .borrow()
            .iter()
            .filter_map(|effect| match effect {
                Effect::UploadEmoji { emoji, name, .. } => Some((*emoji, name.to_string())),
                _ => None,
            })
            .collect()
    }

    /// Chooses `path` in the dialog's file dialog.
    fn choose_file(&self, cx: &mut VisualTestContext, path: PathBuf) {
        self.click(cx, "emoji-add-choose");
        cx.simulate_path_prompt_response(|_| Some(vec![path.clone()]));
        cx.run_until_parked();
        draw(cx);
    }

    fn name_field(&self, cx: &mut VisualTestContext) -> String {
        let dialog = self.dialog(cx);
        dialog.read_with(cx, |dialog, app| {
            dialog.name_input().read(app).text().to_owned()
        })
    }

    fn dialog(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::dialogs::CustomEmojiDialog> {
        let shell = self
            .root
            .read_with(cx, |root, _| root.shell().unwrap().clone());
        shell
            .read_with(cx, |shell, _| shell.custom_emoji_dialog())
            .expect("the custom emoji dialog is open")
    }

    fn live_names(&self, cx: &VisualTestContext) -> Vec<String> {
        self.model.read_with(cx, |m, _| {
            m.core()
                .live_custom_emoji()
                .map(|e| e.name.clone())
                .collect()
        })
    }
}

#[gpui::test]
fn only_admins_and_owners_can_run_the_command_and_no_other_route_opens_the_dialog(
    cx: &mut TestAppContext,
) {
    for (role, expected) in [
        (WorkspaceRole::Member, false),
        (WorkspaceRole::Admin, true),
        (WorkspaceRole::Owner, true),
    ] {
        let (world, cx) = start(cx);
        world.reach_as(cx, role, vec![]);
        let snapshot = world
            .model
            .read_with(cx, |model, _| AppSnapshot::from_core(model.core()));
        assert_eq!(
            availability(CommandId::CustomEmoji, &snapshot).is_enabled(),
            expected,
            "{role:?}"
        );
        assert!(
            !world.on_screen(cx, "sidebar-custom-emoji"),
            "the sidebar has no link: the command is in the Menu"
        );
        if !expected {
            world.root.update_in(cx, |root, window, cx| {
                root.shell().unwrap().update(cx, |shell, cx| {
                    shell.open_dialog(ShellDialog::CustomEmoji, window, cx)
                });
            });
            assert!(
                !world.on_screen(cx, "emoji-admin-close"),
                "members cannot open it"
            );
        }
    }
}

#[gpui::test]
fn the_workspace_menu_lists_custom_emoji_beside_user_groups_and_opens_with_the_keyboard(
    cx: &mut TestAppContext,
) {
    let (world, cx) = admin(cx);
    world.click(cx, "sidebar-menu");
    world.click(cx, "command-menu-tab-Workspace");
    assert!(
        world.on_screen(cx, "command-menu-UserGroups")
            && world.on_screen(cx, "command-menu-CustomEmoji")
    );
    let groups = cx.debug_bounds("command-menu-UserGroups").unwrap();
    let emoji = cx.debug_bounds("command-menu-CustomEmoji").unwrap();
    assert!(
        emoji.top() >= groups.bottom(),
        "after it: {groups:?} then {emoji:?}"
    );

    // Down to Custom emoji (after Workspace settings, Invite people, and User groups, with the
    // first item highlighted when the menu opens) and press Enter.
    cx.simulate_keystrokes("down down down enter");
    assert!(world.on_screen(cx, "emoji-admin-close"));
}

#[gpui::test]
fn the_list_shows_live_emoji_by_name_with_who_added_them_and_the_keyboard_moves_through_it(
    cx: &mut TestAppContext,
) {
    let (world, cx) = admin(cx);
    world.open_dialog(cx);
    assert!(world.on_screen(cx, "emoji-admin-row-0") && world.on_screen(cx, "emoji-admin-row-1"));
    assert!(
        !world.on_screen(cx, "emoji-admin-row-2"),
        "the deleted one is not listed"
    );
    assert!(world.on_screen(cx, "emoji-admin-add") && world.on_screen(cx, "emoji-admin-close"));
    let mut asked = world.fetched();
    asked.sort();
    let mut live = vec![world.id("partyparrot"), world.id("shipit")];
    live.sort();
    assert_eq!(asked, live, "the rows' pictures are asked for");
    world.answer_images(cx, 64, 64);
    assert!(world.on_screen(cx, "emoji-picture"));

    // The list is the first tab stop and has the keyboard: Down moves, Enter renames the row.
    cx.simulate_keystrokes("down enter");
    assert_eq!(
        world.name_field(cx),
        "shipit",
        "the second row is the cursor's now"
    );
    cx.simulate_keystrokes("escape");
    assert!(
        world.on_screen(cx, "emoji-admin-add"),
        "Escape came back to the list"
    );
    assert!(
        world.on_screen(cx, "emoji-admin-close"),
        "and did not close the dialog"
    );

    // Home and End.
    cx.simulate_keystrokes("end enter");
    assert_eq!(world.name_field(cx), "shipit");
    cx.simulate_keystrokes("escape home enter");
    assert_eq!(world.name_field(cx), "partyparrot");
    cx.simulate_keystrokes("escape escape");
    assert!(
        !world.on_screen(cx, "emoji-admin-close"),
        "Escape from the list closes the dialog"
    );
}

#[gpui::test]
fn an_empty_workspace_says_so_and_starts_on_add(cx: &mut TestAppContext) {
    let (world, cx) = start_with(cx, false);
    world.reach_as(cx, WorkspaceRole::Owner, vec![]);
    world.open_dialog(cx);
    assert!(world.on_screen(cx, "emoji-admin-empty"));
    cx.simulate_keystrokes("enter");
    assert!(
        world.on_screen(cx, "emoji-add-choose"),
        "Enter pressed Add custom emoji"
    );
}

#[gpui::test]
fn adding_checks_the_name_as_it_is_typed_and_sends_nothing_until_it_can_be_used(
    cx: &mut TestAppContext,
) {
    let (world, cx) = admin(cx);
    world.open_dialog(cx);
    world.click(cx, "emoji-admin-add");
    assert!(world.on_screen(cx, "emoji-add-choose"));
    assert!(world.on_screen(cx, "emoji-add-submit"));
    world.take_effects();

    // Nothing chosen yet: Enter in the name field sends nothing, whatever the name.
    cx.simulate_keystrokes("tab");
    cx.simulate_input("launch");
    cx.simulate_keystrokes("enter");
    assert_eq!(world.uploads(), vec![], "no picture, nothing to add");

    let path = temp_file("rocket-ship.png", &png(96, 64));
    world.take_effects();
    world.root.update_in(cx, |_, window, _| window.refresh());
    world.choose_file(cx, path.clone());
    assert!(world.on_screen(cx, "emoji-add-file"), "the file is named");
    assert_eq!(
        world.name_field(cx),
        "launch",
        "a name already typed is not overwritten by the file's"
    );
    // The file's own preview is decoded off the main thread.
    assert!(world.on_screen(cx, "emoji-add-preview"));

    // Invalid names say why, and sending stays closed.
    for (typed, said) in [
        ("a", "at least 2"),
        ("has space", "a space"),
        ("shipit", "already an emoji named :shipit:"),
        ("ship", "standard emoji"),
        ("_edge", "start and end"),
        ("x".repeat(33).as_str(), "at most 32"),
    ] {
        let name = world
            .dialog(cx)
            .read_with(cx, |dialog, _| dialog.name_input().clone());
        name.update_in(cx, |input, window, cx| input.focus(window, cx));
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(typed);
        let problem = world.dialog(cx).read_with(cx, |dialog, app| {
            lazy_ui::states::check_emoji_name(
                dialog.name_input().read(app).text(),
                &["shipit", "partyparrot"],
            )
        });
        let lazy_ui::states::EmojiNameCheck::Problem(text) = problem else {
            panic!("{typed:?} should not be accepted");
        };
        assert!(text.contains(said), "{typed:?}: {text}");
        cx.simulate_keystrokes("enter");
        assert_eq!(world.uploads(), vec![], "{typed:?} is not sent");
    }
}

#[gpui::test]
fn a_valid_name_and_a_picture_upload_and_the_new_emoji_joins_the_list(cx: &mut TestAppContext) {
    let (world, cx) = admin(cx);
    world.open_dialog(cx);
    world.click(cx, "emoji-admin-add");
    let path = temp_file("rocket-ship.png", &png(96, 64));
    world.choose_file(cx, path.clone());
    assert_eq!(
        world.name_field(cx),
        "rocket_ship",
        "the file's name, made into a valid emoji name"
    );

    world.take_effects();
    world.click(cx, "emoji-add-submit");
    let uploads = world.uploads();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].1, "rocket_ship");
    assert!(world.on_screen(cx, "emoji-add-progress"), "Uploading…");
    assert_eq!(
        world.state(cx, ActionKind::AddCustomEmoji),
        ActionState::InFlight
    );
    // A second press while it is uploading does nothing.
    cx.simulate_keystrokes("enter");
    assert_eq!(world.uploads().len(), 1);

    // The upload finishes: the server's emoji arrives and the dialog is back on the list.
    let made = CustomEmoji {
        id: uploads[0].0,
        workspace_id: world.workspace,
        name: "rocket_ship".into(),
        created_by: world.me.id,
        created_at: Timestamp::now(),
        deleted: false,
    };
    world.dispatch(
        cx,
        Input::CustomEmojiUploaded {
            emoji: made.id,
            result: Ok(CustomEmojiResponse {
                emoji: made.clone(),
            }),
        },
    );
    draw(cx);
    assert!(world.on_screen(cx, "emoji-admin-add"), "back on the list");
    assert!(world.on_screen(cx, "emoji-admin-row-2"), "with a third row");
    assert_eq!(
        world.live_names(cx),
        vec!["partyparrot", "rocket_ship", "shipit"]
    );
}

#[gpui::test]
fn a_failed_upload_explains_in_place_and_the_same_button_tries_again(cx: &mut TestAppContext) {
    let (world, cx) = admin(cx);
    world.open_dialog(cx);
    world.click(cx, "emoji-admin-add");
    world.choose_file(cx, temp_file("blast_off.png", &png(32, 32)));
    world.click(cx, "emoji-add-submit");
    let (id, _) = world.uploads().pop().unwrap();
    world.take_effects();
    world.dispatch(
        cx,
        Input::CustomEmojiUploaded {
            emoji: id,
            result: Err(TransferError::Request(
                lazy_client_core::RequestError::Rejected(ApiError::new(
                    ErrorCode::UnsupportedMedia,
                    "That image can't be used.",
                )),
            )),
        },
    );
    draw(cx);
    assert!(
        matches!(
            world.state(cx, ActionKind::AddCustomEmoji),
            ActionState::Failed(_)
        ),
        "the core reports the failure"
    );
    assert!(
        world.on_screen(cx, "emoji-add-submit"),
        "the form is still there"
    );
    assert!(!world.on_screen(cx, "emoji-add-progress"));

    world.click(cx, "emoji-add-submit");
    assert_eq!(world.uploads().len(), 1, "Try again uploads once more");
    assert_eq!(
        world.state(cx, ActionKind::AddCustomEmoji),
        ActionState::InFlight
    );
}

#[gpui::test]
fn a_name_that_became_taken_while_typing_is_refused_before_anything_is_sent(
    cx: &mut TestAppContext,
) {
    let (world, cx) = admin(cx);
    world.open_dialog(cx);
    world.click(cx, "emoji-admin-add");
    world.choose_file(cx, temp_file("shipit.png", &png(32, 32)));
    assert_eq!(
        world.name_field(cx),
        "shipit",
        "suggested by the file, but taken"
    );
    world.take_effects();
    world.click(cx, "emoji-add-submit");
    assert_eq!(world.uploads(), vec![], "the Add button was not live");
}

#[gpui::test]
fn escape_steps_back_one_view_at_a_time_and_belongs_to_a_composition_in_the_name(
    cx: &mut TestAppContext,
) {
    let (world, cx) = admin(cx);
    world.open_dialog(cx);
    world.click(cx, "emoji-admin-add");
    assert!(world.on_screen(cx, "emoji-add-choose"));
    cx.simulate_keystrokes("tab");
    let input = world
        .dialog(cx)
        .read_with(cx, |dialog, _| dialog.name_input().clone());
    input.update_in(cx, |input, window, cx| {
        input.replace_and_mark_text_in_range(None, "かな", None, window, cx)
    });
    cx.simulate_keystrokes("escape");
    assert!(
        world.on_screen(cx, "emoji-add-choose"),
        "Escape ended the composition, not the form"
    );
    input.update_in(cx, |input, window, cx| input.unmark_text(window, cx));
    cx.simulate_keystrokes("escape");
    assert!(
        world.on_screen(cx, "emoji-admin-add"),
        "now it steps back to the list"
    );
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "emoji-admin-close"), "and then closes");
}

#[gpui::test]
fn renaming_is_checked_live_and_saved_with_the_keyboard(cx: &mut TestAppContext) {
    let (world, cx) = admin(cx);
    world.open_dialog(cx);
    world.take_effects();
    // Row 1 is shipit.
    cx.simulate_keystrokes("down enter");
    assert_eq!(world.name_field(cx), "shipit");

    // Its own name, unchanged, is not a rename.
    cx.simulate_keystrokes("enter");
    assert!(
        !world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::RenameEmoji { .. })),
        "nothing changed, nothing sent"
    );
    // Another emoji's name is refused.
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("partyparrot");
    cx.simulate_keystrokes("enter");
    assert!(
        !world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::RenameEmoji { .. }))
    );
    // A standard emoji's name is refused.
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("tada");
    cx.simulate_keystrokes("enter");
    assert!(
        !world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::RenameEmoji { .. }))
    );

    // A good one goes out.
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("launch");
    cx.simulate_keystrokes("enter");
    let renames: Vec<_> = world
        .take_effects()
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::RenameEmoji { emoji, request } => Some((emoji, request.name.to_string())),
            _ => None,
        })
        .collect();
    assert_eq!(renames, vec![(world.id("shipit"), "launch".to_owned())]);
    assert_eq!(
        world.state(cx, ActionKind::RenameCustomEmoji(world.id("shipit"))),
        ActionState::InFlight
    );

    // The server's answer returns to the list, which shows the new name in order.
    let renamed = CustomEmoji {
        name: "launch".into(),
        ..world
            .emoji
            .iter()
            .find(|e| e.name == "shipit")
            .unwrap()
            .clone()
    };
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::RenameCustomEmoji(world.id("shipit")),
            result: Ok(ActionOutcome::CustomEmoji(renamed)),
        },
    );
    draw(cx);
    assert!(world.on_screen(cx, "emoji-admin-add"), "back on the list");
    assert_eq!(world.live_names(cx), vec!["launch", "partyparrot"]);
}

#[gpui::test]
fn a_refused_rename_says_why_and_keeps_the_form(cx: &mut TestAppContext) {
    let (world, cx) = admin(cx);
    world.open_dialog(cx);
    cx.simulate_keystrokes("enter ctrl-a");
    cx.simulate_input("launch");
    cx.simulate_keystrokes("enter");
    let id = world.id("partyparrot");
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::RenameCustomEmoji(id),
            result: Err(lazy_client_core::RequestError::Rejected(ApiError::new(
                ErrorCode::NameTaken,
                "That name was just taken.",
            ))),
        },
    );
    draw(cx);
    assert!(world.on_screen(cx, "emoji-rename-save"), "still renaming");
    assert!(matches!(
        world.state(cx, ActionKind::RenameCustomEmoji(id)),
        ActionState::Failed(_)
    ));
    world.take_effects();
    cx.simulate_keystrokes("enter");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::RenameEmoji { .. })),
        "Enter tries again"
    );
}

#[gpui::test]
fn deleting_asks_first_starts_on_cancel_and_changes_nothing_if_cancelled(cx: &mut TestAppContext) {
    let (world, cx) = admin(cx);
    world.open_dialog(cx);
    world.take_effects();
    cx.simulate_keystrokes("delete");
    assert!(world.on_screen(cx, "emoji-delete-confirm"), "asks first");
    assert!(world.on_screen(cx, "emoji-delete-cancel"));
    // The keyboard starts on Cancel, so a reflexive Enter is safe.
    cx.simulate_keystrokes("enter");
    assert!(world.on_screen(cx, "emoji-admin-add"), "back on the list");
    assert!(
        !world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::DeleteEmoji { .. }))
    );
    assert_eq!(world.live_names(cx).len(), 2);

    // Escape cancels too.
    cx.simulate_keystrokes("delete escape");
    assert!(world.on_screen(cx, "emoji-admin-add"));
    assert!(
        !world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::DeleteEmoji { .. }))
    );
}

#[gpui::test]
fn confirming_deletes_the_chosen_emoji_and_a_failure_can_be_retried(cx: &mut TestAppContext) {
    let (world, cx) = admin(cx);
    world.open_dialog(cx);
    cx.simulate_keystrokes("down delete");
    world.take_effects();
    cx.simulate_keystrokes("tab enter");
    let deletes: Vec<_> = world
        .take_effects()
        .into_iter()
        .filter_map(|e| match e {
            Effect::DeleteEmoji { emoji } => Some(emoji),
            _ => None,
        })
        .collect();
    assert_eq!(
        deletes,
        vec![world.id("shipit")],
        "the second row, as the cursor said"
    );
    let kind = ActionKind::DeleteCustomEmoji(world.id("shipit"));
    assert_eq!(world.state(cx, kind), ActionState::InFlight);

    world.dispatch(
        cx,
        Input::ActionResult {
            kind,
            result: Err(lazy_client_core::RequestError::Unknown("timed out".into())),
        },
    );
    draw(cx);
    assert!(
        world.on_screen(cx, "emoji-delete-error"),
        "the reason is on screen"
    );
    cx.simulate_keystrokes("enter");
    cx.simulate_keystrokes("tab enter");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::DeleteEmoji { .. })),
        "and the same button tries again"
    );

    // It works this time: the server says it was deleted, and the list moves on.
    let gone = CustomEmoji {
        deleted: true,
        ..world
            .emoji
            .iter()
            .find(|e| e.name == "shipit")
            .unwrap()
            .clone()
    };
    world.event(cx, 2, Event::EmojiChanged { emoji: gone });
    world.dispatch(
        cx,
        Input::ActionResult {
            kind,
            result: Ok(ActionOutcome::Done),
        },
    );
    draw(cx);
    assert!(world.on_screen(cx, "emoji-admin-add"));
    assert!(!world.on_screen(cx, "emoji-admin-row-1"));
    assert_eq!(world.live_names(cx), vec!["partyparrot"]);
}

#[gpui::test]
fn an_emoji_deleted_by_someone_else_leaves_the_view_that_was_about_it(cx: &mut TestAppContext) {
    let (world, cx) = admin(cx);
    world.open_dialog(cx);
    cx.simulate_keystrokes("enter");
    assert!(world.on_screen(cx, "emoji-rename-save"));
    let gone = CustomEmoji {
        deleted: true,
        ..world
            .emoji
            .iter()
            .find(|e| e.name == "partyparrot")
            .unwrap()
            .clone()
    };
    world.event(cx, 2, Event::EmojiChanged { emoji: gone });
    assert!(world.on_screen(cx, "emoji-admin-add"), "back to the list");
    assert!(!world.on_screen(cx, "emoji-admin-row-1"));
}

#[gpui::test]
fn the_mouse_reaches_every_row_action(cx: &mut TestAppContext) {
    let (world, cx) = admin(cx);
    world.open_dialog(cx);
    world.click(cx, "emoji-admin-rename-1");
    assert_eq!(world.name_field(cx), "shipit");
    world.click(cx, "emoji-rename-cancel");
    assert!(
        world.on_screen(cx, "emoji-admin-add"),
        "Cancel is back on the list"
    );

    world.click(cx, "emoji-admin-delete-0");
    assert!(world.on_screen(cx, "emoji-delete-confirm"));
    world.take_effects();
    world.click(cx, "emoji-delete-confirm");
    assert!(
        world.take_effects().iter().any(
            |e| matches!(e, Effect::DeleteEmoji { emoji } if *emoji == world.id("partyparrot"))
        )
    );
    // The server's answer closes the question; Close closes the dialog.
    let gone = CustomEmoji {
        deleted: true,
        ..world
            .emoji
            .iter()
            .find(|e| e.name == "partyparrot")
            .unwrap()
            .clone()
    };
    world.event(cx, 2, Event::EmojiChanged { emoji: gone });
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::DeleteCustomEmoji(world.id("partyparrot")),
            result: Ok(ActionOutcome::Done),
        },
    );
    draw(cx);
    world.click(cx, "emoji-admin-close");
    assert!(!world.on_screen(cx, "emoji-admin-close"));
}

// ---- A long conversation of custom emoji --------------------------------------------------------------

#[gpui::test]
fn a_hundred_messages_with_five_emoji_scroll_decoding_each_picture_once(cx: &mut TestAppContext) {
    scroll_a_hundred_messages(cx, true);
}

/// The same hundred messages with the emoji names written as plain words, so the cost of the
/// pictures can be read against what scrolling the same text costs without them.
#[gpui::test]
fn a_hundred_plain_messages_of_the_same_shape_for_comparison(cx: &mut TestAppContext) {
    scroll_a_hundred_messages(cx, false);
}

fn scroll_a_hundred_messages(cx: &mut TestAppContext, with_emoji: bool) {
    let (world, cx) = start_with(cx, false);
    // Five distinct emoji, added to the workspace.
    let mut world = world;
    world.emoji = ["one_a", "two_b", "three_c", "four_d", "five_e"]
        .iter()
        .map(|name| CustomEmoji {
            id: EmojiId::generate(),
            workspace_id: world.workspace,
            name: (*name).to_owned(),
            created_by: world.bob.id,
            created_at: Timestamp::now(),
            deleted: false,
        })
        .collect();
    let messages: Vec<Message> = (1..=100)
        .map(|n| {
            let used: Vec<String> = (0..5)
                .map(|k| {
                    let name = &world.emoji[(n as usize + k) % 5].name;
                    if with_emoji {
                        format!(":{name}:")
                    } else {
                        name.clone()
                    }
                })
                .collect();
            world.message(
                &world.bob,
                &format!("message {n} {}", used.join(" some words ")),
                n,
            )
        })
        .collect();
    world.reach(cx, messages);

    let started = Instant::now();
    let mut answered = world.answer_images(cx, 128, 128).len();
    let mut slowest = std::time::Duration::ZERO;
    let steps = 60u32;
    for _ in 0..steps {
        let frame = Instant::now();
        cx.simulate_event(gpui::ScrollEvent {
            position: point(px(900.), px(400.)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(600.))),
            ..Default::default()
        });
        draw(cx);
        answered += world.answer_images(cx, 128, 128).len();
        slowest = slowest.max(frame.elapsed());
    }
    let total = started.elapsed();

    // And back down over pictures that are all decoded already: drawing alone.
    let mut cached_total = std::time::Duration::ZERO;
    let mut slowest_cached = std::time::Duration::ZERO;
    for _ in 0..steps {
        let frame = Instant::now();
        cx.simulate_event(gpui::ScrollEvent {
            position: point(px(900.), px(400.)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-600.))),
            ..Default::default()
        });
        draw(cx);
        let took = frame.elapsed();
        cached_total += took;
        slowest_cached = slowest_cached.max(took);
    }
    let (decoded, generation) = world.store(cx);
    let kind = if with_emoji {
        "100 messages x 5 custom emoji"
    } else {
        "100 plain messages (the same text, no emoji)"
    };
    eprintln!(
        "{kind}, {steps} scroll steps up: {total:?} in all, slowest \
         {slowest:?}; back down over decoded pictures: {cached_total:?} in all ({:?} a step), \
         slowest {slowest_cached:?}; {decoded} pictures decoded, {generation} re-layouts",
        cached_total / steps
    );
    let expected = if with_emoji { 5 } else { 0 };
    assert_eq!(
        answered, expected,
        "each picture was fetched once, however many messages use it"
    );
    assert_eq!(decoded, expected, "and decoded once");
    assert_eq!(
        generation, expected as u64,
        "and the lists were laid out again once for each"
    );
    assert_eq!(world.take_fetches(), vec![], "nothing is asked for again");
    // Only a step that takes tens of seconds would be a decode on the main thread or a layout of
    // every row on every frame; the numbers above are for a debug build on a shared machine.
    assert!(slowest < std::time::Duration::from_secs(20), "{slowest:?}");
    assert!(
        slowest_cached < std::time::Duration::from_secs(20),
        "{slowest_cached:?}"
    );
}
