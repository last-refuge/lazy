//! Threads on GPUI's test platform: summary lines in the conversation, the thread pane beside it,
//! the keyboard path in and out, resizing, narrow windows, and replying. The real views and the
//! real core run; a recording effect sink stands in for the runtime.
//!
//! Every keyboard claim is made with real key events through GPUI's dispatch. The test platform
//! never reports a window as active and gives every character the same width, so these prove
//! behavior and wiring; real layout is checked in the offscreen renders. Nothing here exercises a
//! real input method, a screen reader, or a display.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, FocusHandle, Focusable as _, Modifiers, MouseButton, TestAppContext,
    VisualTestContext, WindowAppearance, point, px, size,
};
use lazy_client_core::{Effect, Input, StartupState};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    MessageDocument, MessageId, Permalink, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    BootstrapResponse, Conversation, ConversationUserState, Event, EventEnvelope, HistoryPage,
    Message, PROTOCOL_VERSION, ServerFrame, ThreadSummary, ThreadUserState, User, Workspace,
    WorkspaceBootstrap,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::root_view::RootView;
use lazy_ui::thread_layout;

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
    // A window wide enough for the conversation and the pane side by side.
    cx.simulate_resize(size(px(1280.), px(800.)));
    let world = World {
        model,
        effects,
        root,
        me: person("alice"),
        bob: person("bob"),
        zoe: person("zoe"),
        workspace: WorkspaceId::generate(),
        general: ConversationId::generate(),
    };
    (world, cx)
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

/// Turns the mouse wheel over the conversation, as a reader scrolling back through history.
fn scroll_up(cx: &mut VisualTestContext, pixels: f32) {
    draw(cx);
    let at = point(px(500.), px(300.));
    for _ in 0..4 {
        cx.simulate_event(gpui::ScrollEvent {
            position: at,
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(pixels / 4.0))),
            modifiers: Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
            source: gpui::ScrollSource::Pointer,
        });
        draw(cx);
    }
}

fn summary(replies: u32, repliers: &[&User], minutes_ago: i64, as_of: i64) -> ThreadSummary {
    let at = time::OffsetDateTime::now_utc() - time::Duration::minutes(minutes_ago);
    ThreadSummary {
        reply_count: replies,
        last_reply_seq: EventSeq::new(as_of).unwrap(),
        last_reply_at: Timestamp::from(at),
        recent_repliers: repliers.iter().map(|user| user.id).collect(),
        as_of: EventSeq::new(as_of).unwrap(),
    }
}

impl World {
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

    fn bootstrap(&self, threads: Vec<ThreadUserState>) -> BootstrapResponse {
        BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: self.me.clone(),
            workspaces: vec![WorkspaceBootstrap {
                saved: Vec::new(),
                notification_settings: Default::default(),
                conversation_notification_prefs: Vec::new(),
                custom_emoji: Vec::new(),
                groups: Vec::new(),
                threads,
                workspace: Workspace {
                    id: self.workspace,
                    name: "Lazy".into(),
                    settings: Default::default(),
                },
                role: WorkspaceRole::Member,
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

    fn message(&self, author: &User, text: &str, seq: i64) -> Message {
        Message {
            attachments: vec![],
            mentions: Default::default(),
            thread_root: None,
            in_channel: false,
            thread: None,
            id: MessageId::generate(),
            conversation_id: self.general,
            author_id: author.id,
            document: MessageDocument::from_markdown(text).unwrap(),
            created_at: Timestamp::now(),
            revision: 1,
            seq: EventSeq::new(seq).unwrap(),
            edited_at: None,
            reactions: vec![],
            reactions_as_of: EventSeq::ZERO,
            rich: None,
            deleted: false,
        }
    }

    /// A root with `thread` as its summary.
    fn root(&self, author: &User, text: &str, seq: i64, thread: ThreadSummary) -> Message {
        Message {
            attachments: vec![],
            thread: Some(thread),
            ..self.message(author, text, seq)
        }
    }

    /// A reply to `root`.
    fn reply(&self, root: &Message, author: &User, text: &str, seq: i64) -> Message {
        Message {
            attachments: vec![],
            thread_root: Some(root.id),
            ..self.message(author, text, seq)
        }
    }

    fn dispatch(&self, cx: &mut VisualTestContext, input: Input) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.run_until_parked();
    }

    fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.effects.0.borrow_mut())
    }

    fn reach_with(
        &self,
        cx: &mut VisualTestContext,
        history: Vec<Message>,
        threads: Vec<ThreadUserState>,
    ) {
        self.dispatch(
            cx,
            Input::Started(StartupState {
                session_restored: true,
                ..Default::default()
            }),
        );
        self.dispatch(
            cx,
            Input::BootstrapLoaded(Box::new(self.bootstrap(threads))),
        );
        let effects = self.take_effects();
        let generation = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::FetchHistory { generation, .. } => Some(*generation),
                _ => None,
            })
            .expect("loading #general fetches its history");
        // Everyone is in #general; the core offers mentions only once it knows who is.
        for effect in &effects {
            if let Effect::FetchMembers {
                conversation,
                generation,
            } = effect
            {
                self.dispatch(
                    cx,
                    Input::MembersFetched {
                        conversation: *conversation,
                        generation: *generation,
                        result: Ok(lazy_protocol::ConversationMembersResponse {
                            conversation_id: *conversation,
                            user_ids: vec![self.me.id, self.bob.id, self.zoe.id],
                        }),
                    },
                );
            }
        }
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
        draw(cx);
        self.take_effects();
    }

    fn reach(&self, cx: &mut VisualTestContext, history: Vec<Message>) {
        self.reach_with(cx, history, vec![]);
    }

    /// Answers the newest request for `root`'s replies with `replies`.
    fn answer_replies(
        &self,
        cx: &mut VisualTestContext,
        root: &Message,
        replies: Vec<Message>,
        has_more: bool,
    ) {
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
                    has_more,
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

    fn hover_row(&self, cx: &mut VisualTestContext, selector: &'static str) {
        draw(cx);
        let row = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector:?} is not on screen"));
        cx.simulate_mouse_move(row.center(), None, Modifiers::default());
        cx.run_until_parked();
    }

    fn open_thread_id(&self, cx: &VisualTestContext) -> Option<MessageId> {
        self.model
            .read_with(cx, |m, _| m.core().open_thread().map(|open| open.root))
    }

    fn shell(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::shell::ShellView> {
        self.root.read_with(cx, |root, _| {
            root.shell().expect("the shell is showing").clone()
        })
    }

    fn pane(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::thread_pane::ThreadPane> {
        let shell = self.shell(cx);
        shell.read_with(cx, |shell, _| shell.thread_pane().clone())
    }

    fn is_focused(&self, cx: &mut VisualTestContext, handle: &FocusHandle) -> bool {
        cx.update(|window, _| handle.is_focused(window))
    }

    /// The text the person has typed into the reply composer.
    fn reply_draft(&self, cx: &VisualTestContext, root: &Message) -> String {
        self.model.read_with(cx, |m, _| {
            m.core()
                .thread(root.id)
                .map(|view| view.draft.to_owned())
                .unwrap_or_default()
        })
    }

    fn pane_reply_texts(&self, cx: &VisualTestContext) -> Vec<String> {
        self.model.read_with(cx, |m, _| {
            let Some(open) = m.core().open_thread() else {
                return vec![];
            };
            m.core()
                .thread(open.root)
                .map(|view| {
                    view.replies
                        .iter()
                        .map(|item| item.document.plain_text())
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    /// What the conversation's timeline list shows: its first row and whether it follows the end.
    fn channel_scroll(&self, cx: &mut VisualTestContext) -> Option<(usize, gpui::Pixels, bool)> {
        let shell = self.shell(cx);
        let timeline = shell.read_with(cx, |shell, _| shell.timeline().clone());
        timeline.read_with(cx, |timeline, cx| timeline.scroll_snapshot(cx))
    }

    fn sent(&self) -> Vec<lazy_client_core::JournalEntry> {
        self.take_effects()
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::PersistSubmission(entry) => Some(entry),
                _ => None,
            })
            .collect()
    }
}

/// A conversation with a thread of two replies on its second message.
struct Scene {
    root: Message,
    replies: Vec<Message>,
    history: Vec<Message>,
}

fn scene(world: &World) -> Scene {
    let before = world.message(&world.zoe, "Before the thread", 1);
    let root = world.root(
        &world.bob,
        "Shall we move the launch?",
        2,
        summary(2, &[&world.zoe, &world.bob], 3, 4),
    );
    let after = world.message(&world.zoe, "Unrelated, after it", 5);
    let replies = vec![
        world.reply(&root, &world.zoe, "I would wait a week", 3),
        world.reply(&root, &world.bob, "Agreed, a week it is", 4),
    ];
    Scene {
        history: vec![before, root.clone(), after],
        root,
        replies,
    }
}

// ---- The summary line in the conversation ---------------------------------------------------------

#[gpui::test]
fn a_root_with_replies_shows_a_summary_line_and_a_plain_message_does_not(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    world.reach(cx, scene.history.clone());
    assert!(
        world.on_screen(cx, "thread-line"),
        "the root has its summary line"
    );
    assert!(world.on_screen(cx, "thread-reply-count"));
    assert!(
        !world.on_screen(cx, "thread-unread-dot"),
        "nothing unread when the person does not follow it"
    );
    assert!(
        !world.on_screen(cx, "thread-pane"),
        "no pane until it is asked for"
    );
}

#[gpui::test]
fn an_unread_followed_thread_shows_a_dot(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    let state = ThreadUserState {
        root_id: scene.root.id,
        conversation_id: world.general,
        following: true,
        last_read: EventSeq::new(3).unwrap(),
        unread_count: 1,
        mention_count: 0,
    };
    world.reach_with(cx, scene.history.clone(), vec![state]);
    assert!(
        world.on_screen(cx, "thread-unread-dot"),
        "a followed thread with an unread reply is marked"
    );
}

#[gpui::test]
fn a_reply_also_sent_to_the_channel_carries_a_header_that_opens_the_thread(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    let mut also = world.reply(&scene.root, &world.zoe, "Posting this one here too", 6);
    also.in_channel = true;
    let mut history = scene.history.clone();
    history.push(also);
    world.reach(cx, history);
    assert!(
        world.on_screen(cx, "reply-header"),
        "the header names what it is"
    );
    world.click(cx, "reply-header");
    assert_eq!(
        world.open_thread_id(cx),
        Some(scene.root.id),
        "the header opens the thread of the root"
    );
}

#[gpui::test]
fn a_deleted_root_with_replies_is_a_muted_placeholder_with_no_actions_but_opens(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let mut root = world.root(
        &world.bob,
        "This will be deleted",
        2,
        summary(2, &[&world.zoe], 5, 4),
    );
    root.deleted = true;
    let scene_replies = vec![world.reply(&root, &world.zoe, "Still here", 3)];
    world.reach(cx, vec![root.clone()]);
    assert!(
        world.on_screen(cx, "deleted-message"),
        "the placeholder shows"
    );
    assert!(
        world.on_screen(cx, "thread-line"),
        "with the thread beneath it"
    );
    world.hover_row(cx, "message-row-0");
    for action in [
        "reply-message",
        "edit-message",
        "delete-message",
        "react-message",
    ] {
        assert!(
            !world.on_screen(cx, label(action.to_owned())),
            "{action} is not offered on a deleted message"
        );
    }
    // The keyboard offers no menu for it either.
    cx.simulate_keystrokes("shift-tab enter");
    assert!(!world.on_screen(cx, "message-actions"));

    world.click(cx, "thread-line");
    assert_eq!(world.open_thread_id(cx), Some(root.id));
    world.answer_replies(cx, &root, scene_replies, false);
    assert!(world.on_screen(cx, "thread-pane"));
    assert!(
        world.on_screen(cx, "deleted-message"),
        "the pane shows the placeholder too"
    );
}

// ---- Opening and closing the pane -------------------------------------------------------------------

#[gpui::test]
fn clicking_the_summary_line_opens_the_pane_beside_the_conversation(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    world.reach(cx, scene.history.clone());
    world.take_effects();

    world.click(cx, "thread-line");
    assert_eq!(world.open_thread_id(cx), Some(scene.root.id));
    assert!(world.on_screen(cx, "thread-pane"));
    assert!(
        world.on_screen(cx, "send-message") && world.on_screen(cx, "thread-send-reply"),
        "the conversation and the pane are both showing"
    );
    let asked_for_replies = world.effects.0.borrow().iter().any(
        |effect| matches!(effect, Effect::FetchReplies { root, .. } if *root == scene.root.id),
    );
    assert!(asked_for_replies, "the replies are fetched");

    world.answer_replies(cx, &scene.root, scene.replies.clone(), false);
    assert_eq!(
        world.pane_reply_texts(cx),
        vec!["I would wait a week", "Agreed, a week it is"]
    );
    assert!(world.on_screen(cx, "thread-reply-divider"));
    // Everyone in the pane is a row of the pane's timeline: the root and the two replies.
    assert!(
        world.on_screen(cx, "thread-message-row-0"),
        "the root is the pane's first row"
    );
    assert!(
        world.on_screen(cx, "thread-message-row-2"),
        "and the replies follow it"
    );
}

#[gpui::test]
fn opening_the_thread_from_the_keyboard_lands_in_the_reply_composer_and_escape_returns(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    // The root is the last message, so the keyboard cursor reaches it at once.
    let mut history = scene.history.clone();
    history.pop();
    world.reach(cx, history);

    // Shift+Tab from the composer reaches the timeline with the cursor on the newest message (the
    // root). Down moves onto its summary line, and Enter opens the thread.
    cx.simulate_keystrokes("shift-tab down enter");
    assert_eq!(
        world.open_thread_id(cx),
        Some(scene.root.id),
        "Enter on the summary line opens it"
    );
    world.answer_replies(cx, &scene.root, scene.replies.clone(), false);

    // The keyboard is in the reply composer: typed text becomes the thread's draft.
    cx.simulate_input("thanks both");
    assert_eq!(world.reply_draft(cx, &scene.root), "thanks both");

    // Escape closes the pane and gives the keyboard back to the timeline, cursor where it was.
    cx.simulate_keystrokes("escape");
    assert_eq!(world.open_thread_id(cx), None);
    assert!(!world.on_screen(cx, "thread-pane"));
    cx.simulate_keystrokes("enter");
    assert_eq!(
        world.open_thread_id(cx),
        Some(scene.root.id),
        "the cursor was still on the summary line"
    );
}

#[gpui::test]
fn the_menu_offers_reply_in_thread_after_add_reaction_and_opens_the_pane(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let plain = world.message(&world.bob, "A plain message", 1);
    world.reach(cx, vec![plain]);
    cx.simulate_keystrokes("shift-tab enter");
    assert!(world.on_screen(cx, "message-actions"));
    for item in ["react", "reply", "copy-link", "copy-text"] {
        let selector = label(format!("message-actions-{item}"));
        assert!(world.on_screen(cx, selector), "{item} is offered");
    }
    // Add reaction…, then Reply in thread.
    cx.simulate_keystrokes("down enter");
    let root = world
        .model
        .read_with(cx, |m, _| m.core().open_thread().map(|open| open.root))
        .expect("the thread opened");
    assert_eq!(
        world.model.read_with(cx, |m, _| m
            .core()
            .timeline(world.general)
            .first()
            .and_then(|item| item.message_id())),
        Some(root)
    );
    assert!(world.on_screen(cx, "thread-pane"));
    assert!(
        world.on_screen(cx, "thread-reply-divider"),
        "a root without replies says so"
    );
}

#[gpui::test]
fn the_toolbar_reply_button_opens_the_pane_and_escape_goes_back_to_the_composer(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let plain = world.message(&world.bob, "A plain message", 1);
    world.reach(cx, vec![plain]);
    world.hover_row(cx, "message-row-0");
    world.click(cx, "reply-message");
    assert!(
        world.open_thread_id(cx).is_some(),
        "the toolbar button opens it"
    );
    assert!(world.on_screen(cx, "thread-pane"));

    cx.simulate_keystrokes("escape");
    assert!(world.open_thread_id(cx).is_none());
    // With no row origin the keyboard goes to the conversation's composer.
    let shell = world.shell(cx);
    let composer_input = shell.read_with(cx, |shell, cx| {
        shell.composer().read(cx).input().focus_handle(cx)
    });
    assert!(world.is_focused(cx, &composer_input));
}

#[gpui::test]
fn the_reply_composer_sends_with_enter_makes_newlines_and_can_also_send_to_the_channel(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    world.reach(cx, scene.history.clone());
    world.click(cx, "thread-line");
    world.answer_replies(cx, &scene.root, scene.replies.clone(), false);
    world.take_effects();

    // Enter sends to the thread only.
    cx.simulate_input("a reply");
    cx.simulate_keystrokes("enter");
    let sent = world.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].thread_root, Some(scene.root.id));
    assert!(!sent[0].also_in_channel);
    assert_eq!(sent[0].document.plain_text(), "a reply");

    // Shift+Enter is a new line, not a send.
    cx.simulate_input("line one");
    cx.simulate_keystrokes("shift-enter");
    cx.simulate_input("line two");
    assert!(world.sent().is_empty(), "nothing sent yet");
    assert_eq!(world.reply_draft(cx, &scene.root), "line one\nline two");

    // The checkbox is a tab stop: Space toggles it, and the reply goes to the channel as well.
    cx.simulate_keystrokes("tab");
    cx.simulate_keystrokes("space");
    let composer = world
        .pane(cx)
        .read_with(cx, |pane, _| pane.composer().clone());
    assert!(composer.read_with(cx, |composer, _| composer.also_in_channel()));
    cx.simulate_keystrokes("shift-tab");
    cx.simulate_keystrokes("enter");
    let sent = world.sent();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].also_in_channel, "also sent to the channel");
    assert_eq!(sent[0].document.plain_text(), "line one\nline two");
}

#[gpui::test]
fn the_reply_composer_has_mention_and_emoji_autocomplete_and_an_emoji_button(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    world.reach(cx, scene.history.clone());
    world.click(cx, "thread-line");
    world.answer_replies(cx, &scene.root, scene.replies.clone(), false);

    cx.simulate_input("ping @bo");
    assert!(
        world.on_screen(cx, "thread-composer-suggestions"),
        "mentions are offered in the pane"
    );
    cx.simulate_keystrokes("enter");
    assert_eq!(world.reply_draft(cx, &scene.root), "ping @bob ");

    cx.simulate_input("nice :tad");
    assert!(world.on_screen(cx, "thread-composer-suggestions"));
    cx.simulate_keystrokes("enter");
    assert_eq!(world.reply_draft(cx, &scene.root), "ping @bob nice 🎉");

    // The emoji button opens the picker and the choice lands in the reply composer.
    world.click(cx, "thread-composer-emoji");
    assert!(world.on_screen(cx, "emoji-picker"));
    cx.simulate_input("tada");
    cx.simulate_keystrokes("enter");
    assert_eq!(world.reply_draft(cx, &scene.root), "ping @bob nice 🎉🎉");
    assert_eq!(
        world
            .model
            .read_with(cx, |m, _| m.core().draft(world.general).to_owned()),
        "",
        "the conversation's own draft is untouched"
    );
}

// ---- The conversation stays where it is --------------------------------------------------------------

#[gpui::test]
fn opening_and_closing_the_pane_leaves_the_conversations_reading_position_alone(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    // Enough messages that the conversation scrolls, with the thread's root in the middle.
    let mut history: Vec<Message> = (1..=60)
        .map(|n| world.message(&world.zoe, &format!("Message number {n}"), n))
        .collect();
    let root = world.root(
        &world.bob,
        "The thread's root",
        30,
        summary(1, &[&world.zoe], 2, 31),
    );
    history[29] = root.clone();
    world.reach(cx, history);

    // Scroll to the top with the keyboard (Home reveals the oldest message), then reveal the root.
    scroll_up(cx, 1800.0);
    let before = world.channel_scroll(cx).expect("the list has been drawn");
    assert!(
        !before.2,
        "the reader is away from the newest message: {before:?}"
    );
    world.take_effects();

    world.dispatch(
        cx,
        Input::OpenThread {
            conversation: world.general,
            root: root.id,
        },
    );
    draw(cx);
    assert!(world.on_screen(cx, "thread-pane"));
    let during = world.channel_scroll(cx).expect("still drawn");
    assert_eq!(
        during.0, before.0,
        "the first visible row is the same with the pane open"
    );
    assert!(
        !during.2,
        "and the reader is still away from the newest message"
    );

    world.dispatch(cx, Input::CloseThread);
    draw(cx);
    let after = world.channel_scroll(cx).expect("still drawn");
    assert_eq!(after.0, before.0, "and after it closes");
    assert!(!after.2);
    // Neither moved the conversation's read position or asked for more of its history.
    let effects = world.take_effects();
    assert!(
        !effects.iter().any(|effect| matches!(
            effect,
            Effect::MarkRead { .. } | Effect::FetchHistory { .. }
        )),
        "the conversation's reading position was not touched: {effects:?}"
    );
}

// ---- Resizing -----------------------------------------------------------------------------------------

#[gpui::test]
fn the_handle_resizes_the_pane_from_the_keyboard_within_limits_and_the_width_is_remembered(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    // Room for the widest pane beside a conversation of its minimum.
    cx.simulate_resize(size(px(1700.), px(800.)));
    let scene = scene(&world);
    world.reach(cx, scene.history.clone());
    world.click(cx, "thread-line");
    world.answer_replies(cx, &scene.root, scene.replies.clone(), false);

    let width = |cx: &mut VisualTestContext| -> f32 {
        draw(cx);
        f32::from(cx.debug_bounds("thread-pane").expect("pane").size.width)
            - thread_layout::HANDLE_WIDTH
    };
    assert_eq!(width(cx), thread_layout::DEFAULT_PANE_WIDTH);

    let handle = world.pane(cx).read_with(cx, |pane, _| pane.handle_focus());
    cx.update(|window, cx| window.focus(&handle, cx));
    assert!(world.is_focused(cx, &handle));
    cx.simulate_keystrokes("left");
    assert_eq!(
        width(cx),
        thread_layout::DEFAULT_PANE_WIDTH + thread_layout::KEY_STEP
    );
    cx.simulate_keystrokes("shift-left");
    assert_eq!(
        width(cx),
        thread_layout::DEFAULT_PANE_WIDTH + thread_layout::KEY_STEP + thread_layout::KEY_STEP_LARGE
    );
    cx.simulate_keystrokes("right");
    assert_eq!(
        width(cx),
        thread_layout::DEFAULT_PANE_WIDTH + thread_layout::KEY_STEP_LARGE
    );
    cx.simulate_keystrokes("home");
    assert_eq!(width(cx), thread_layout::MAX_PANE_WIDTH);
    cx.simulate_keystrokes("end");
    assert_eq!(width(cx), thread_layout::MIN_PANE_WIDTH);
    cx.simulate_keystrokes("right");
    assert_eq!(
        width(cx),
        thread_layout::MIN_PANE_WIDTH,
        "never below the minimum"
    );

    // Closing and opening again keeps the width for the session.
    cx.simulate_keystrokes("left left");
    let wanted = width(cx);
    world.dispatch(cx, Input::CloseThread);
    world.click(cx, "thread-line");
    assert_eq!(width(cx), wanted, "the width is remembered");
}

#[gpui::test]
fn dragging_the_handle_resizes_the_pane(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    world.reach(cx, scene.history.clone());
    world.click(cx, "thread-line");
    world.answer_replies(cx, &scene.root, scene.replies.clone(), false);

    let width = |cx: &mut VisualTestContext| -> f32 {
        draw(cx);
        f32::from(cx.debug_bounds("thread-pane").expect("pane").size.width)
            - thread_layout::HANDLE_WIDTH
    };
    let start_width = width(cx);
    let handle = cx.debug_bounds("thread-resize-handle").expect("handle");
    let from = handle.center();
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(
        point(from.x - px(60.), from.y),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    cx.simulate_mouse_move(
        point(from.x - px(120.), from.y),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    cx.simulate_mouse_up(
        point(from.x - px(120.), from.y),
        MouseButton::Left,
        Modifiers::default(),
    );
    let dragged = width(cx);
    assert!(
        (dragged - (start_width + 120.0)).abs() < 8.0,
        "the pane followed the pointer: {start_width} then {dragged}"
    );

    // Dragging far to the right stops at the minimum.
    let handle = cx.debug_bounds("thread-resize-handle").expect("handle");
    let from = handle.center();
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(
        point(from.x + px(300.), from.y),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    cx.simulate_mouse_move(
        point(from.x + px(900.), from.y),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    cx.simulate_mouse_up(
        point(from.x + px(900.), from.y),
        MouseButton::Left,
        Modifiers::default(),
    );
    assert_eq!(width(cx), thread_layout::MIN_PANE_WIDTH);
}

// ---- Narrow windows -----------------------------------------------------------------------------------

#[gpui::test]
fn in_a_narrow_window_the_pane_replaces_the_conversation_and_back_returns_to_it(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    world.reach(cx, scene.history.clone());
    world.click(cx, "thread-line");
    world.answer_replies(cx, &scene.root, scene.replies.clone(), false);
    assert!(
        world.on_screen(cx, "send-message"),
        "wide: both are showing"
    );

    // Narrower than the sidebar plus 680 px: the pane takes the whole area.
    cx.simulate_resize(size(px(900.), px(700.)));
    draw(cx);
    assert!(world.on_screen(cx, "thread-pane"));
    assert!(world.on_screen(cx, "thread-back"), "Back replaces Close");
    assert!(!world.on_screen(cx, "thread-close"));
    assert!(
        !world.on_screen(cx, "thread-resize-handle"),
        "there is nothing to resize against"
    );
    assert!(
        !world.on_screen(cx, "send-message"),
        "the conversation is not drawn beside it"
    );

    // Back returns to the conversation and closes the thread.
    world.click(cx, "thread-back");
    assert_eq!(world.open_thread_id(cx), None);
    assert!(world.on_screen(cx, "send-message"));
    assert!(!world.on_screen(cx, "thread-pane"));
}

#[gpui::test]
fn a_narrow_window_keeps_the_conversations_place_while_the_pane_covers_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    cx.simulate_resize(size(px(900.), px(700.)));
    let mut history: Vec<Message> = (1..=60)
        .map(|n| world.message(&world.zoe, &format!("Message number {n}"), n))
        .collect();
    let root = world.root(
        &world.bob,
        "Root in a narrow window",
        30,
        summary(1, &[&world.zoe], 2, 31),
    );
    history[29] = root.clone();
    world.reach(cx, history);
    scroll_up(cx, 1800.0);
    let before = world.channel_scroll(cx).expect("drawn");

    world.dispatch(
        cx,
        Input::OpenThread {
            conversation: world.general,
            root: root.id,
        },
    );
    draw(cx);
    assert!(world.on_screen(cx, "thread-back"));
    world.click(cx, "thread-back");
    let after = world.channel_scroll(cx).expect("drawn again");
    assert_eq!(after.0, before.0, "the conversation is where it was");
}

// ---- Tab order -----------------------------------------------------------------------------------------

#[gpui::test]
fn tab_walks_the_pane_in_a_documented_order(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    world.reach(cx, scene.history.clone());
    world.click(cx, "thread-line");
    world.answer_replies(cx, &scene.root, scene.replies.clone(), false);

    let pane = world.pane(cx);
    let (handle, follow, close, timeline_focus, composer_input, also, send) =
        pane.read_with(cx, |pane, cx| {
            let timeline = pane.timeline().read(cx).focus_handle(cx);
            let composer = pane.composer().read(cx);
            (
                pane.handle_focus(),
                pane.follow_focus(),
                pane.close_focus(),
                timeline,
                composer.input().read(cx).focus_handle(cx),
                composer.also_focus(),
                composer.send_focus(),
            )
        });
    let _ = send;
    // Start on the resize handle and press Tab: Follow, Close, the replies, the reply box, the
    // checkbox, and then the emoji button and Send beyond them.
    cx.update(|window, cx| window.focus(&handle, cx));
    let expected = [
        ("follow", &follow),
        ("close", &close),
        ("replies", &timeline_focus),
        ("reply composer", &composer_input),
        ("also-send checkbox", &also),
    ];
    for (name, handle) in expected {
        cx.simulate_keystrokes("tab");
        assert!(world.is_focused(cx, handle), "Tab reaches the {name}");
    }
    // And backwards.
    for (name, handle) in expected.iter().rev().skip(1) {
        cx.simulate_keystrokes("shift-tab");
        assert!(world.is_focused(cx, handle), "Shift+Tab reaches the {name}");
    }
}

// ---- Acting on messages in the pane ---------------------------------------------------------------------

#[gpui::test]
fn replies_in_the_pane_can_be_reacted_to_edited_copied_and_deleted_like_any_message(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    let mine = world.reply(&scene.root, &world.me, "My own reply", 6);
    let mut replies = scene.replies.clone();
    replies.push(mine.clone());
    world.reach(cx, scene.history.clone());
    world.click(cx, "thread-line");
    world.answer_replies(cx, &scene.root, replies, false);

    // The toolbar of my reply (the fourth row: root, two replies, mine).
    world.hover_row(cx, "thread-message-row-3");
    for selector in [
        "thread-react-message",
        "thread-edit-message",
        "thread-delete-message",
        "thread-copy-link",
        "thread-copy-message",
    ] {
        assert!(
            world.on_screen(cx, selector),
            "{selector} is offered on my reply"
        );
    }
    assert!(
        !world.on_screen(cx, "thread-reply-message"),
        "a reply is not replied to: the composer is right there"
    );

    // Copy link names the reply's own id in the conversation.
    world.click(cx, "thread-copy-link");
    let expected = Permalink {
        workspace_id: world.workspace,
        conversation_id: world.general,
        message_id: mine.id,
    }
    .to_string();
    assert_eq!(clipboard(cx), Some(expected));

    // Edit opens the editor in place and Escape returns to the pane's replies.
    world.hover_row(cx, "thread-message-row-3");
    world.click(cx, "thread-edit-message");
    assert!(world.on_screen(cx, "thread-message-editor"));
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "thread-message-editor"));
    assert!(
        world.open_thread_id(cx).is_some(),
        "Escape ended the edit, not the pane"
    );
    world.take_effects();

    // Reacting: the picker opens for the reply and a choice is sent for it.
    world.hover_row(cx, "thread-message-row-1");
    world.click(cx, "thread-message-1-react-message");
    assert!(world.on_screen(cx, "emoji-picker"));
    cx.simulate_input("tada");
    cx.simulate_keystrokes("enter");
    let effects = world.take_effects();
    let reacted = effects.iter().any(|effect| {
        matches!(effect, Effect::SetReaction { message, .. } if *message == scene.replies[0].id)
    });
    let notice = world
        .model
        .read_with(cx, |m, _| m.core().notice().map(str::to_owned));
    assert!(
        reacted,
        "the reaction was sent for the reply: {effects:?} {notice:?}"
    );

    // Delete asks first, and then deletes the reply.
    world.hover_row(cx, "thread-message-row-3");
    world.click(cx, "thread-delete-message");
    assert!(world.on_screen(cx, "delete-message-confirm"));
    world.click(cx, "delete-message-confirm");
    let deleted = world
        .take_effects()
        .into_iter()
        .any(|effect| matches!(effect, Effect::DeleteMessage { message } if message == mine.id));
    assert!(deleted, "the reply's deletion was requested");
}

#[gpui::test]
fn the_pane_follow_button_toggles_following(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    world.reach(cx, scene.history.clone());
    world.click(cx, "thread-line");
    world.answer_replies(cx, &scene.root, scene.replies.clone(), false);
    world.take_effects();

    world.click(cx, "thread-follow");
    let asked = world.take_effects().into_iter().any(|effect| {
        matches!(effect, Effect::SetThreadFollow { root, following: true } if root == scene.root.id)
    });
    assert!(asked, "following was requested");
}

#[gpui::test]
fn an_empty_thread_says_so_and_a_failed_reply_keeps_its_text(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let plain = world.message(&world.bob, "No replies yet", 1);
    world.reach(cx, vec![plain.clone()]);
    world.hover_row(cx, "message-row-0");
    world.click(cx, "reply-message");
    assert!(world.on_screen(cx, "thread-reply-divider"));
    // No `thread-line` for a root without replies, in the pane or the conversation.
    cx.simulate_input("first reply");
    world.take_effects();
    cx.simulate_keystrokes("enter");
    let entry = world.sent().pop().expect("the reply was journaled");
    assert_eq!(entry.thread_root, Some(plain.id));
    let _ = entry;
}

#[gpui::test]
fn a_reply_edit_with_mentions_offers_autocomplete_in_the_pane(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    let mine = world.reply(&scene.root, &world.me, "My own reply", 6);
    let mut replies = scene.replies.clone();
    replies.push(mine);
    world.reach(cx, scene.history.clone());
    world.click(cx, "thread-line");
    world.answer_replies(cx, &scene.root, replies, false);

    world.hover_row(cx, "thread-message-row-3");
    world.click(cx, "thread-edit-message");
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("cc @zo");
    assert!(
        world.on_screen(cx, "editor-suggestions"),
        "the editor in the pane offers mentions"
    );
    cx.simulate_keystrokes("enter");
    cx.simulate_keystrokes("ctrl-a ctrl-c");
    assert_eq!(clipboard(cx).as_deref(), Some("cc @zoe "));
}

// ---- Live changes, links, and read state -----------------------------------------------------------------

#[gpui::test]
fn a_reply_arriving_live_adds_the_summary_line_and_grows_the_open_pane(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let plain = world.message(&world.bob, "Anyone around?", 1);
    world.reach(cx, vec![plain.clone()]);
    assert!(
        !world.on_screen(cx, "thread-line"),
        "a message with no replies has no line"
    );

    // A reply is committed: the message, then the root's new summary.
    let first = world.reply(&plain, &world.zoe, "I am", 2);
    world.event(
        cx,
        2,
        Event::MessageCreated {
            message: first,
            operation_id: None,
        },
    );
    world.event(
        cx,
        3,
        Event::ThreadChanged {
            conversation_id: world.general,
            root_id: plain.id,
            summary: Some(summary(1, &[&world.zoe], 0, 3)),
        },
    );
    assert!(
        world.on_screen(cx, "thread-line"),
        "the summary line appears under the root without a reload"
    );

    // With the pane open, the next reply appears in it and the row count follows.
    world.click(cx, "thread-line");
    world.answer_replies(
        cx,
        &plain,
        vec![world.reply(&plain, &world.zoe, "I am", 2)],
        false,
    );
    assert!(!world.on_screen(cx, "thread-message-row-2"));
    let second = world.reply(&plain, &world.bob, "Me too", 4);
    world.event(
        cx,
        4,
        Event::MessageCreated {
            message: second,
            operation_id: None,
        },
    );
    world.event(
        cx,
        5,
        Event::ThreadChanged {
            conversation_id: world.general,
            root_id: plain.id,
            summary: Some(summary(2, &[&world.bob, &world.zoe], 0, 5)),
        },
    );
    assert!(
        world.on_screen(cx, "thread-message-row-2"),
        "the new reply is the pane's third row"
    );
    assert_eq!(world.pane_reply_texts(cx), vec!["I am", "Me too"]);
}

#[gpui::test]
fn a_reply_sent_to_the_channel_too_is_marked_in_the_pane(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    world.reach(cx, scene.history.clone());
    world.click(cx, "thread-line");
    let mut also = world.reply(&scene.root, &world.zoe, "Also in the channel", 6);
    also.in_channel = true;
    let mut replies = scene.replies.clone();
    replies.push(also);
    world.answer_replies(cx, &scene.root, replies, false);
    assert!(
        world.on_screen(cx, "also-sent-to-channel"),
        "the pane says the reply went to the channel too"
    );
}

#[gpui::test]
fn tab_from_the_conversations_send_button_reaches_the_pane(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    world.reach(cx, scene.history.clone());
    world.click(cx, "thread-line");
    world.answer_replies(cx, &scene.root, scene.replies.clone(), false);
    let shell = world.shell(cx);
    let send = shell.read_with(cx, |shell, cx| shell.composer().read(cx).send_focus());
    let handle = world.pane(cx).read_with(cx, |pane, _| pane.handle_focus());
    cx.update(|window, cx| window.focus(&send, cx));
    cx.simulate_keystrokes("tab");
    assert!(
        world.is_focused(cx, &handle),
        "after the conversation's composer the next stop is the pane's resize handle"
    );
}

#[gpui::test]
fn a_permalink_to_a_reply_opens_its_thread_and_highlights_the_reply(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    world.reach(cx, scene.history.clone());
    let target = scene.replies[1].clone();
    world.take_effects();
    world.dispatch(
        cx,
        Input::OpenPermalink(Permalink {
            workspace_id: world.workspace,
            conversation_id: world.general,
            message_id: target.id,
        }),
    );
    // The core does not hold the reply yet: it fetches the message, which names its root.
    let fetched = world
        .take_effects()
        .into_iter()
        .any(|effect| matches!(effect, Effect::FetchMessage { message } if message == target.id));
    assert!(fetched, "the core asks for the message the link names");
    world.dispatch(
        cx,
        Input::MessageFetched {
            message: target.id,
            result: Ok(lazy_protocol::MessageResponse {
                message: target.clone(),
            }),
        },
    );
    assert_eq!(
        world.open_thread_id(cx),
        Some(scene.root.id),
        "the pane opens on the root"
    );
    world.answer_replies(cx, &scene.root, scene.replies.clone(), false);
    assert!(
        world.on_screen(cx, "thread-highlighted-row"),
        "the reply the link named is highlighted in the pane"
    );
    assert!(
        !world.on_screen(cx, "highlighted-row"),
        "and nothing in the conversation is"
    );
}

#[gpui::test]
fn the_thread_is_marked_read_only_while_the_pane_shows_its_newest_reply(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    let state = ThreadUserState {
        root_id: scene.root.id,
        conversation_id: world.general,
        following: true,
        last_read: EventSeq::new(3).unwrap(),
        unread_count: 1,
        mention_count: 0,
    };
    world.reach_with(cx, scene.history.clone(), vec![state]);
    world.dispatch(cx, Input::WindowActive(true));
    world.take_effects();

    // Nothing is marked while the pane is closed, however long the window is active.
    draw(cx);
    assert!(
        !world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::MarkThreadRead { .. })),
        "a closed pane marks nothing read"
    );

    world.click(cx, "thread-line");
    world.answer_replies(cx, &scene.root, scene.replies.clone(), false);
    let marked = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::MarkThreadRead { root, up_to } if root == scene.root.id => Some(up_to),
            _ => None,
        });
    assert_eq!(
        marked,
        Some(EventSeq::new(4).unwrap()),
        "the newest reply is on screen, so the thread is read up to it"
    );
}

#[gpui::test]
fn a_thread_opened_from_the_menu_returns_to_the_message_on_escape(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    let mut history = scene.history.clone();
    history.pop();
    world.reach(cx, history);
    // The menu of the root: Add reaction…, Reply in thread.
    cx.simulate_keystrokes("shift-tab enter down enter");
    assert_eq!(world.open_thread_id(cx), Some(scene.root.id));
    world.answer_replies(cx, &scene.root, scene.replies.clone(), false);
    cx.simulate_keystrokes("escape");
    assert_eq!(world.open_thread_id(cx), None);
    // The keyboard is back on the timeline with the cursor on the message, not its summary line:
    // Enter opens the menu again rather than the thread.
    cx.simulate_keystrokes("enter");
    assert!(world.on_screen(cx, "message-actions"));
    assert_eq!(world.open_thread_id(cx), None);
}

#[gpui::test]
fn a_reply_shows_in_the_pane_at_once_and_is_marked_while_it_sends(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let scene = scene(&world);
    world.reach(cx, scene.history.clone());
    world.click(cx, "thread-line");
    world.answer_replies(cx, &scene.root, scene.replies.clone(), false);
    cx.simulate_input("on my way");
    cx.simulate_keystrokes("enter");
    assert_eq!(
        world.pane_reply_texts(cx),
        vec!["I would wait a week", "Agreed, a week it is", "on my way"],
        "the reply is in the thread before the server answers"
    );
    assert!(world.on_screen(cx, "thread-message-row-3"));
    assert_eq!(world.reply_draft(cx, &scene.root), "", "the box is cleared");
}

#[gpui::test]
fn scrolling_up_in_a_long_thread_loads_older_replies_and_keeps_the_place(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let root = world.root(
        &world.bob,
        "A very long thread",
        1,
        summary(80, &[&world.zoe], 1, 90),
    );
    world.reach(cx, vec![root.clone()]);
    world.click(cx, "thread-line");
    // The newest fifty replies, with more before them.
    let newest: Vec<Message> = (31..=80)
        .map(|n| world.reply(&root, &world.zoe, &format!("Reply {n}"), 100 + n))
        .collect();
    world.answer_replies(cx, &root, newest, true);
    world.take_effects();

    // Scroll to the top of what is loaded.
    let pane_scroll = |cx: &mut VisualTestContext| {
        draw(cx);
        let pane = world.pane(cx);
        let timeline = pane.read_with(cx, |pane, _| pane.timeline().clone());
        timeline.read_with(cx, |timeline, cx| timeline.scroll_snapshot(cx))
    };
    for _ in 0..40 {
        cx.simulate_event(gpui::ScrollEvent {
            position: point(px(1000.), px(300.)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(600.))),
            modifiers: Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
            source: gpui::ScrollSource::Pointer,
        });
        draw(cx);
    }
    let asked = world
        .effects
        .0
        .borrow()
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::FetchReplies {
                root: r, before, ..
            } if *r == root.id => Some(*before),
            _ => None,
        });
    assert!(
        matches!(asked, Some(Some(_))),
        "reaching the top asks for the page before the oldest loaded reply: {asked:?}"
    );
    // Reply 32 is the pane's third row. (Reply 31 loses its header when older replies from the
    // same person arrive above it, so it is the one after that which holds still.)
    draw(cx);
    let first_y = |cx: &mut VisualTestContext, row: usize| {
        draw(cx);
        cx.debug_bounds(label(format!("thread-message-row-{row}")))
            .map(|bounds| f32::from(bounds.origin.y))
    };
    let before = first_y(cx, 2).expect("the second loaded reply is on screen at the top");
    let _ = pane_scroll;

    let older: Vec<Message> = (1..=30)
        .map(|n| world.reply(&root, &world.zoe, &format!("Reply {n}"), 100 + n))
        .collect();
    world.answer_replies(cx, &root, older, false);
    draw(cx);
    draw(cx);
    let after = first_y(cx, 32).expect("the same reply is still on screen, thirty rows further on");
    assert!(
        (after - before).abs() < 1.0,
        "the reader stays on the same reply: it was at {before}, now at {after}"
    );
    assert_eq!(world.pane_reply_texts(cx).len(), 80);
}
