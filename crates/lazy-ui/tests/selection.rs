//! Selecting and copying text in the timeline and the thread pane, on GPUI's test platform: the
//! real views and the real core, real mouse and key events, a recording effect sink.
//!
//! What this proves: the selection model over loaded messages, the mouse and keyboard paths into
//! it, what a copy writes, and that a selection survives scrolling, new messages, and older
//! history. What it cannot: the test platform lays every character out at one width, left to
//! right, with no input method, so pointer positions on real proportional, right-to-left, or
//! bidirectional text, and real IME composition, are not exercised here (the renders and the
//! pure tests in `selection.rs` cover the rest). Nothing here talks to a screen reader.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AppContext as _, Entity, EntityInputHandler as _, Focusable as _, Modifiers, MouseButton,
    MouseDownEvent, MouseUpEvent, Pixels, Point, TestAppContext, VisualTestContext,
    WindowAppearance, point, px, size,
};
use jiff::tz::TimeZone;
use lazy_client_core::{Effect, Input, StartupState};
use lazy_domain::mention::{MapDirectory, MentionTarget};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    MessageDocument, MessageId, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    BootstrapResponse, Conversation, ConversationUserState, Event, EventEnvelope, HistoryPage,
    Message, PROTOCOL_VERSION, ServerFrame, ThreadSummary, User, Workspace, WorkspaceBootstrap,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::root_view::RootView;
use lazy_ui::time_fmt::{clock_label, to_zoned};

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

fn person(name: &str, display: &str) -> User {
    User {
        id: UserId::generate(),
        username: Username::parse(name).unwrap(),
        display_name: DisplayName::parse(display).unwrap(),
        is_bot: false,
        deactivated: false,
        avatar_version: 0,
        status: None,
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
    cx.simulate_resize(size(px(1280.), px(800.)));
    let world = World {
        model,
        effects,
        root,
        me: person("alice", "Alice"),
        bob: person("bob", "Bob"),
        zoe: person("zoe", "Zoe"),
        workspace: WorkspaceId::generate(),
        general: ConversationId::generate(),
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

/// The time label a message shows, as the timeline computes it.
fn time_of(message: &Message) -> String {
    clock_label(&to_zoned(message.created_at, &TimeZone::system()))
}

impl World {
    fn directory(&self) -> MapDirectory {
        let mut directory = MapDirectory::new();
        for user in [&self.me, &self.bob, &self.zoe] {
            directory.insert(user.username.as_str(), MentionTarget::User { id: user.id });
        }
        directory
    }

    fn bootstrap(&self) -> BootstrapResponse {
        BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: self.me.clone(),
            workspaces: vec![WorkspaceBootstrap {
                saved: Vec::new(),
                notification_settings: Default::default(),
                conversation_notification_prefs: Vec::new(),
                custom_emoji: Vec::new(),
                groups: Vec::new(),
                threads: Vec::new(),
                workspace: Workspace {
                    id: self.workspace,
                    name: "Lazy".into(),
                    settings: Default::default(),
                },
                role: WorkspaceRole::Member,
                conversations: vec![Conversation {
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
                }],
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

    /// A message from `author` whose text is `text` (composer source), `seq`th in the log, a
    /// minute apart from its neighbors.
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

    fn dispatch(&self, cx: &mut VisualTestContext, input: Input) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.run_until_parked();
    }

    fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.effects.0.borrow_mut())
    }

    fn reach(&self, cx: &mut VisualTestContext, history: Vec<Message>) {
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
                    messages: history,
                    has_more: false,
                }),
            },
        );
        draw(cx);
        self.take_effects();
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

    fn shell(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::shell::ShellView> {
        self.root.read_with(cx, |root, _| {
            root.shell().expect("the shell is showing").clone()
        })
    }

    fn timeline(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::timeline::TimelineView> {
        let shell = self.shell(cx);
        shell.read_with(cx, |shell, _| shell.timeline().clone())
    }

    fn pane_timeline(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::timeline::TimelineView> {
        let shell = self.shell(cx);
        shell.read_with(cx, |shell, cx| {
            shell.thread_pane().read(cx).timeline().clone()
        })
    }

    /// What a copy would write from the conversation's timeline right now.
    fn selected(&self, cx: &mut VisualTestContext) -> Option<String> {
        draw(cx);
        let timeline = self.timeline(cx);
        timeline.read_with(cx, |timeline, cx| timeline.selected_text(cx))
    }

    fn pane_selected(&self, cx: &mut VisualTestContext) -> Option<String> {
        draw(cx);
        let timeline = self.pane_timeline(cx);
        timeline.read_with(cx, |timeline, cx| timeline.selected_text(cx))
    }

    /// The window position of the text at `offset` of row `row` of the conversation.
    fn at(&self, cx: &mut VisualTestContext, row: usize, offset: usize) -> Point<Pixels> {
        draw(cx);
        let timeline = self.timeline(cx);
        timeline
            .read_with(cx, |timeline, cx| timeline.text_point(row, offset, cx))
            .unwrap_or_else(|| panic!("row {row} offset {offset} is not on screen"))
    }

    fn pane_at(&self, cx: &mut VisualTestContext, row: usize, offset: usize) -> Point<Pixels> {
        draw(cx);
        let timeline = self.pane_timeline(cx);
        timeline
            .read_with(cx, |timeline, cx| timeline.text_point(row, offset, cx))
            .unwrap_or_else(|| panic!("pane row {row} offset {offset} is not on screen"))
    }

    /// Presses at `from`, drags through the middle to `to`, and lets go.
    fn drag(&self, cx: &mut VisualTestContext, from: Point<Pixels>, to: Point<Pixels>) {
        let held = Some(MouseButton::Left);
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
        let middle = point((from.x + to.x) / 2.0, (from.y + to.y) / 2.0);
        cx.simulate_mouse_move(middle, held, Modifiers::default());
        cx.simulate_mouse_move(to, held, Modifiers::default());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
    }

    /// A click that is the `count`th in a row (a double click is 2).
    fn click_times(
        &self,
        cx: &mut VisualTestContext,
        at: Point<Pixels>,
        count: usize,
        modifiers: Modifiers,
    ) {
        for n in 1..=count {
            cx.simulate_event(MouseDownEvent {
                position: at,
                modifiers,
                button: MouseButton::Left,
                click_count: n,
                first_mouse: false,
            });
            cx.simulate_event(MouseUpEvent {
                position: at,
                modifiers,
                button: MouseButton::Left,
                click_count: n,
            });
        }
        cx.run_until_parked();
    }

    fn composer_input(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::text_input::TextInput> {
        let shell = self.shell(cx);
        shell.read_with(cx, |shell, cx| shell.composer().read(cx).input().clone())
    }

    fn timeline_has_the_keyboard(&self, cx: &mut VisualTestContext) -> bool {
        let timeline = self.timeline(cx);
        let handle = timeline.read_with(cx, |timeline, cx| timeline.focus_handle(cx));
        cx.update(|window, _| handle.is_focused(window))
    }

    fn composer_has_the_keyboard(&self, cx: &mut VisualTestContext) -> bool {
        let input = self.composer_input(cx);
        let handle = input.read_with(cx, |input, cx| input.focus_handle(cx));
        cx.update(|window, _| handle.is_focused(window))
    }
}

/// Three short messages by three people.
fn trio(world: &World) -> Vec<Message> {
    vec![
        world.message(&world.me, "First message here", 1),
        world.message(&world.bob, "Second one", 2),
        world.message(&world.zoe, "Third", 3),
    ]
}

// ---- Mouse ---------------------------------------------------------------------------------------

#[gpui::test]
fn dragging_inside_one_message_selects_and_copies_just_that_text(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, trio(&world));
    assert_eq!(
        world.selected(cx),
        None,
        "nothing is selected to begin with"
    );

    // "message" in the first message: bytes 6..13.
    let (from, to) = (world.at(cx, 0, 6), world.at(cx, 0, 13));
    world.drag(cx, from, to);
    assert_eq!(world.selected(cx).as_deref(), Some("message"));
}

#[gpui::test]
fn dragging_across_messages_copies_each_as_author_and_time_then_its_text(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let history = trio(&world);
    world.reach(cx, history.clone());

    // From "message here" in the first, to "Sec" of the second.
    let (from, to) = (world.at(cx, 0, 6), world.at(cx, 1, 3));
    world.drag(cx, from, to);
    let expected = format!(
        "Alice  {}\nmessage here\n\nBob  {}\nSec",
        time_of(&history[0]),
        time_of(&history[1])
    );
    assert_eq!(world.selected(cx), Some(expected.clone()));
    // The mouse selection took the keyboard to the timeline, so Ctrl+C copies it.
    assert!(world.timeline_has_the_keyboard(cx));
    cx.simulate_keystrokes("ctrl-c");
    assert_eq!(clipboard(cx), Some(expected));
}

#[gpui::test]
fn dragging_upward_selects_the_same_text_as_dragging_downward(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, trio(&world));
    let (from, to) = (world.at(cx, 1, 6), world.at(cx, 0, 6));
    world.drag(cx, from, to);
    let up = world.selected(cx).expect("selected");
    assert!(up.starts_with("Alice  "), "{up}");
    assert!(up.contains("message here"));
    assert!(up.ends_with("Second"), "{up}");
}

#[gpui::test]
fn a_double_click_selects_a_word_and_a_triple_click_a_message_and_a_drag_grows_by_words(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let history = vec![
        world.message(&world.me, "alpha beta gamma", 1),
        world.message(&world.bob, "delta epsilon", 2),
    ];
    world.reach(cx, history);

    let beta = world.at(cx, 0, 8);
    world.click_times(cx, beta, 2, Modifiers::default());
    assert_eq!(world.selected(cx).as_deref(), Some("beta"));

    world.click_times(cx, beta, 3, Modifiers::default());
    assert_eq!(world.selected(cx).as_deref(), Some("alpha beta gamma"));

    // A double click that keeps going grows by whole words.
    let held = Some(MouseButton::Left);
    for n in 1..=2 {
        cx.simulate_event(MouseDownEvent {
            position: beta,
            modifiers: Modifiers::default(),
            button: MouseButton::Left,
            click_count: n,
            first_mouse: false,
        });
        if n == 1 {
            cx.simulate_event(MouseUpEvent {
                position: beta,
                modifiers: Modifiers::default(),
                button: MouseButton::Left,
                click_count: 1,
            });
        }
    }
    let gamma = world.at(cx, 0, 13);
    cx.simulate_mouse_move(gamma, held, Modifiers::default());
    cx.simulate_mouse_up(gamma, MouseButton::Left, Modifiers::default());
    assert_eq!(
        world.selected(cx).as_deref(),
        Some("beta gamma"),
        "the dragged-over word is taken whole"
    );
}

#[gpui::test]
fn shift_click_extends_the_selection_to_the_click(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let history = trio(&world);
    world.reach(cx, history.clone());
    let start_at = world.at(cx, 0, 0);
    let first_end = world.at(cx, 0, 5);
    world.drag(cx, start_at, first_end);
    assert_eq!(world.selected(cx).as_deref(), Some("First"));

    let third = world.at(cx, 2, 5);
    world.click_times(cx, third, 1, Modifiers::shift());
    let expected = format!(
        "Alice  {}\nFirst message here\n\nBob  {}\nSecond one\n\nZoe  {}\nThird",
        time_of(&history[0]),
        time_of(&history[1]),
        time_of(&history[2])
    );
    assert_eq!(world.selected(cx), Some(expected));
}

#[gpui::test]
fn a_plain_click_selects_nothing_and_clears_what_was_selected(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, trio(&world));
    let (from, to) = (world.at(cx, 0, 0), world.at(cx, 0, 5));
    world.drag(cx, from, to);
    assert!(world.selected(cx).is_some());

    let elsewhere = world.at(cx, 1, 3);
    cx.simulate_click(elsewhere, Modifiers::default());
    assert_eq!(world.selected(cx), None, "a click leaves nothing selected");
    // The click gave the keyboard back to the composer (the drag had taken it), and clicking text
    // again does not take it away.
    assert!(world.composer_has_the_keyboard(cx));
    let third = world.at(cx, 2, 2);
    cx.simulate_click(third, Modifiers::default());
    assert!(world.composer_has_the_keyboard(cx));
}

#[gpui::test]
fn a_click_on_a_link_still_opens_it_and_a_drag_over_one_selects_its_text(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let text = "see [one](https://example.com/one) and [two](https://example.com/two) now";
    world.reach(cx, vec![world.message(&world.bob, text, 1)]);
    // The message as copied: "see one and two now".
    let on_one = world.at(cx, 0, 5);
    cx.simulate_click(on_one, Modifiers::default());
    assert_eq!(cx.opened_url().as_deref(), Some("https://example.com/one"));
    assert_eq!(world.selected(cx), None, "a click selects nothing");

    // A drag across the second link selects its text and does not open it.
    let (from, to) = (world.at(cx, 0, 12), world.at(cx, 0, 15));
    world.drag(cx, from, to);
    assert_eq!(world.selected(cx).as_deref(), Some("two"));
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com/one"),
        "the second link was not opened by the drag"
    );
}

// ---- Keyboard ------------------------------------------------------------------------------------

#[gpui::test]
fn ctrl_a_selects_everything_loaded_including_messages_scrolled_out_of_view(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let history: Vec<Message> = (1..=80)
        .map(|n| world.message(&world.zoe, &format!("Message number {n}"), n))
        .collect();
    world.reach(cx, history);

    // The list shows the newest messages; the oldest are far above the window.
    cx.simulate_keystrokes("shift-tab");
    cx.simulate_keystrokes("ctrl-a");
    let all = world.selected(cx).expect("everything is selected");
    assert!(
        all.contains("Message number 1\n"),
        "the first, scrolled away, is in it"
    );
    assert!(all.contains("Message number 40"));
    assert!(all.ends_with("Message number 80"), "and the newest");
    assert_eq!(
        all.matches("Zoe  ").count(),
        80,
        "each message has its header"
    );

    cx.simulate_keystrokes("ctrl-c");
    assert_eq!(clipboard(cx).as_deref(), Some(all.as_str()));
}

#[gpui::test]
fn shift_up_and_down_grow_and_shrink_the_selection_by_whole_messages(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let history = trio(&world);
    world.reach(cx, history.clone());
    let header = |n: usize| {
        let who = ["Alice", "Bob", "Zoe"][n];
        format!("{who}  {}\n", time_of(&history[n]))
    };

    // The cursor starts on the newest message: Shift+Up selects it, then the one before.
    cx.simulate_keystrokes("shift-tab shift-up");
    assert_eq!(
        world.selected(cx),
        Some("Third".to_owned()),
        "one message: its text"
    );
    cx.simulate_keystrokes("shift-up");
    assert_eq!(
        world.selected(cx),
        Some(format!("{}Second one\n\n{}Third", header(1), header(2)))
    );
    cx.simulate_keystrokes("shift-up");
    assert_eq!(
        world.selected(cx),
        Some(format!(
            "{}First message here\n\n{}Second one\n\n{}Third",
            header(0),
            header(1),
            header(2)
        ))
    );
    // Shift+Down takes the oldest back out.
    cx.simulate_keystrokes("shift-down");
    assert_eq!(
        world.selected(cx),
        Some(format!("{}Second one\n\n{}Third", header(1), header(2)))
    );
    // Escape clears it, and the keyboard stays on the timeline (it was never moved by the mouse).
    cx.simulate_keystrokes("escape");
    assert_eq!(world.selected(cx), None);
    assert!(world.timeline_has_the_keyboard(cx));
}

#[gpui::test]
fn escape_after_a_mouse_selection_clears_it_and_returns_the_keyboard_to_the_composer(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx, trio(&world));
    let (from, to) = (world.at(cx, 0, 0), world.at(cx, 0, 5));
    world.drag(cx, from, to);
    assert!(world.timeline_has_the_keyboard(cx));
    cx.simulate_keystrokes("escape");
    assert_eq!(world.selected(cx), None);
    assert!(
        world.composer_has_the_keyboard(cx),
        "typing works again at once"
    );
}

// ---- Staying put -----------------------------------------------------------------------------------

#[gpui::test]
fn a_selection_survives_scrolling_away_new_messages_and_older_history(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let history: Vec<Message> = (21..=80)
        .map(|n| world.message(&world.zoe, &format!("Message number {n}"), n))
        .collect();
    world.reach(cx, history.clone());

    // Select "number 75" in the second-to-last message.
    let target = history.len() - 2;
    let (from, to) = (world.at(cx, target, 8), world.at(cx, target, 17));
    world.drag(cx, from, to);
    assert_eq!(world.selected(cx).as_deref(), Some("number 79"));

    // Scroll far away: the selected row is no longer on screen, and the selection is unchanged.
    for _ in 0..8 {
        cx.simulate_event(gpui::ScrollEvent {
            position: point(px(500.), px(300.)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(600.))),
            modifiers: Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
            source: gpui::ScrollSource::Pointer,
        });
        draw(cx);
    }
    assert_eq!(
        world.selected(cx).as_deref(),
        Some("number 79"),
        "scrolled away"
    );

    // A new message arrives at the end.
    let newest = world.message(&world.bob, "Fresh from Bob", 90);
    world.event(
        cx,
        90,
        Event::MessageCreated {
            message: newest,
            operation_id: None,
        },
    );
    assert_eq!(
        world.selected(cx).as_deref(),
        Some("number 79"),
        "a message arrived"
    );

    // Older history is prepended above everything.
    world.dispatch(
        cx,
        Input::LoadOlder {
            conversation: world.general,
        },
    );
    let generation = world
        .effects
        .0
        .borrow()
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::FetchHistory { generation, .. } => Some(*generation),
            _ => None,
        });
    if let Some(generation) = generation {
        let older: Vec<Message> = (1..=20)
            .map(|n| world.message(&world.zoe, &format!("Older number {n}"), n))
            .collect();
        world.dispatch(
            cx,
            Input::HistoryLoaded {
                conversation: world.general,
                generation,
                result: Ok(HistoryPage {
                    messages: older,
                    has_more: false,
                }),
            },
        );
    }
    assert_eq!(
        world.selected(cx).as_deref(),
        Some("number 79"),
        "older history loaded"
    );
}

#[gpui::test]
fn a_selection_that_reaches_a_deleted_message_is_dropped_without_harm(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let history = trio(&world);
    world.reach(cx, history.clone());
    let (from, to) = (world.at(cx, 0, 0), world.at(cx, 1, 4));
    world.drag(cx, from, to);
    assert!(world.selected(cx).is_some());

    let mut tombstone = history[1].clone();
    tombstone.deleted = true;
    world.event(
        cx,
        10,
        Event::MessageDeleted {
            message: tombstone,
            had_mentions: Default::default(),
        },
    );
    assert_eq!(
        world.selected(cx),
        None,
        "its end is gone, so the selection is"
    );
    // The timeline still works.
    cx.simulate_keystrokes("ctrl-a");
}

#[gpui::test]
fn dragging_past_the_bottom_edge_scrolls_and_keeps_extending_the_selection(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let history: Vec<Message> = (1..=80)
        .map(|n| world.message(&world.zoe, &format!("Message number {n}"), n))
        .collect();
    world.reach(cx, history);
    // Scroll to the top so there is plenty below.
    for _ in 0..40 {
        cx.simulate_event(gpui::ScrollEvent {
            position: point(px(500.), px(300.)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(900.))),
            modifiers: Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
            source: gpui::ScrollSource::Pointer,
        });
        draw(cx);
    }
    let timeline = world.timeline(cx);
    let first_row = timeline
        .read_with(cx, |timeline, cx| timeline.scroll_snapshot(cx))
        .expect("drawn")
        .0;
    let top = world.at(cx, first_row, 0);

    // Press on the first visible text, then hold the pointer at the very bottom of the window.
    cx.simulate_mouse_down(top, MouseButton::Left, Modifiers::default());
    let low = point(px(600.), px(780.));
    cx.simulate_mouse_move(
        point(top.x + px(30.), top.y + px(30.)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    cx.simulate_mouse_move(low, Some(MouseButton::Left), Modifiers::default());
    for _ in 0..12 {
        cx.executor().advance_clock(Duration::from_millis(40));
        cx.run_until_parked();
        draw(cx);
    }
    cx.simulate_mouse_up(low, MouseButton::Left, Modifiers::default());
    let after = timeline
        .read_with(cx, |timeline, cx| timeline.scroll_snapshot(cx))
        .expect("drawn")
        .0;
    assert!(
        after > first_row,
        "holding at the edge scrolled the list: {first_row} then {after}"
    );
    let selected = world.selected(cx).expect("selected");
    let last_selected_number: usize = selected
        .rsplit("Message number ")
        .next()
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .expect("the selection ends in a message");
    assert!(
        last_selected_number > first_row + 12,
        "the selection reached messages that were below the window: {last_selected_number}"
    );
}

// ---- What gets copied ------------------------------------------------------------------------------

#[gpui::test]
fn links_mentions_and_code_copy_as_their_text_and_the_copy_action_matches(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let history = vec![world.message(
        &world.bob,
        "ping @zoe about `a_b()` and [docs](https://example.com) **now**",
        1,
    )];
    world.reach(cx, history);
    let whole = "ping @Zoe about a_b() and docs now";
    let end = whole.len();
    let (from, to) = (world.at(cx, 0, 0), world.at(cx, 0, end));
    world.drag(cx, from, to);
    assert_eq!(
        world.selected(cx).as_deref(),
        Some(whole),
        "a mention as the name its pill shows, a link and code and bold as their text"
    );

    // The message's own Copy action writes the same text.
    cx.simulate_keystrokes("escape");
    cx.simulate_keystrokes("shift-tab enter");
    let timeline_menu = "message-actions-copy-text";
    draw(cx);
    let bounds = cx
        .debug_bounds(timeline_menu)
        .expect("the menu offers Copy text");
    cx.simulate_click(bounds.center(), Modifiers::default());
    assert_eq!(clipboard(cx).as_deref(), Some(whole));
}

#[gpui::test]
fn text_of_every_kind_is_selected_by_whole_clusters_and_copied_exactly(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    // Emoji (a ZWJ family and a flag), combining marks, Hebrew and Arabic (right to left), a
    // bidirectional run, and CJK.
    let text =
        "go 👨‍👩‍👧 🇯🇵 e\u{301}a\u{308} שלום עולם مرحبا بالعالم mixed עברית and English 你好世界 done";
    let history = vec![world.message(&world.bob, text, 1)];
    world.reach(cx, history);

    // Dragging over all of it copies it exactly, in logical order, nothing split.
    let (from, to) = (world.at(cx, 0, 0), world.at(cx, 0, text.len()));
    world.drag(cx, from, to);
    assert_eq!(world.selected(cx).as_deref(), Some(text));

    // A triple click is the same whole message.
    cx.simulate_keystrokes("escape");
    let inside = world.at(cx, 0, 3);
    world.click_times(cx, inside, 3, Modifiers::default());
    assert_eq!(world.selected(cx).as_deref(), Some(text));

    // A drag that begins and ends inside clusters moves to their edges: the family emoji is
    // copied whole or not at all, and so is the combining pair.
    cx.simulate_keystrokes("escape");
    let family = text.find('👨').unwrap();
    let family_len = "👨‍👩‍👧".len();
    let mid_family = family + 4; // inside the first person
    let after_family = family + family_len;
    let (from, to) = (
        world.at(cx, 0, mid_family),
        world.at(cx, 0, after_family + 1),
    );
    world.drag(cx, from, to);
    let copied = world.selected(cx).expect("selected");
    assert!(
        copied.starts_with("👨‍👩‍👧") || copied.starts_with(' '),
        "the emoji is whole or left out: {copied:?}"
    );
    assert!(
        !copied.contains('\u{200d}') || copied.contains("👨‍👩‍👧"),
        "no dangling joiner"
    );

    // A double click on a CJK character selects that character (the word rules treat each
    // ideograph as a word), on Hebrew selects the word.
    cx.simulate_keystrokes("escape");
    let cjk = text.find('你').unwrap();
    let on_cjk = world.at(cx, 0, cjk);
    world.click_times(cx, on_cjk, 2, Modifiers::default());
    assert_eq!(world.selected(cx).as_deref(), Some("你"));
    cx.simulate_keystrokes("escape");
    let hebrew = text.find('ש').unwrap();
    let on_hebrew = world.at(cx, 0, hebrew + 2);
    world.click_times(cx, on_hebrew, 2, Modifiers::default());
    assert_eq!(world.selected(cx).as_deref(), Some("שלום"));
}

// ---- Elsewhere in the window ---------------------------------------------------------------------------

#[gpui::test]
fn the_toolbar_the_actions_menu_and_reaction_chips_still_work_with_a_selection(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx, trio(&world));
    let (from, to) = (world.at(cx, 0, 0), world.at(cx, 0, 5));
    world.drag(cx, from, to);
    assert!(world.selected(cx).is_some());

    // The hover toolbar's Copy still copies the message.
    let row = cx.debug_bounds("message-row-1").expect("row");
    cx.simulate_mouse_move(row.center(), None, Modifiers::default());
    draw(cx);
    let copy = cx
        .debug_bounds("message-1-copy-message")
        .expect("the toolbar is showing");
    cx.simulate_click(copy.center(), Modifiers::default());
    assert_eq!(clipboard(cx).as_deref(), Some("Second one"));

    // The keyboard menu still opens on the cursor row.
    cx.simulate_keystrokes("shift-tab enter");
    draw(cx);
    assert!(
        cx.debug_bounds("message-actions").is_some(),
        "the menu opens"
    );
}

#[gpui::test]
fn there_is_one_selection_at_a_time_across_the_timeline_the_pane_and_the_composer(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let mut root = world.message(&world.bob, "Root of a thread", 1);
    root.thread = Some(ThreadSummary {
        reply_count: 1,
        last_reply_seq: EventSeq::new(3).unwrap(),
        last_reply_at: Timestamp::now(),
        recent_repliers: vec![world.zoe.id],
        as_of: EventSeq::new(3).unwrap(),
    });
    let reply = Message {
        attachments: vec![],
        thread_root: Some(root.id),
        ..world.message(&world.zoe, "A reply in the pane", 3)
    };
    world.reach(
        cx,
        vec![root.clone(), world.message(&world.me, "After it", 4)],
    );
    world.dispatch(
        cx,
        Input::OpenThread {
            conversation: world.general,
            root: root.id,
        },
    );
    let generation = world
        .effects
        .0
        .borrow()
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::FetchReplies { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("the thread asks for its replies");
    world.dispatch(
        cx,
        Input::RepliesLoaded {
            root: root.id,
            generation,
            result: Ok(HistoryPage {
                messages: vec![reply],
                has_more: false,
            }),
        },
    );
    draw(cx);

    // Select in the conversation.
    let (from, to) = (world.at(cx, 0, 0), world.at(cx, 0, 4));
    world.drag(cx, from, to);
    assert_eq!(world.selected(cx).as_deref(), Some("Root"));

    // Select in the pane (its second row is the reply): the conversation's selection ends.
    let (from, to) = (world.pane_at(cx, 1, 2), world.pane_at(cx, 1, 7));
    world.drag(cx, from, to);
    assert_eq!(world.pane_selected(cx).as_deref(), Some("reply"));
    assert_eq!(world.selected(cx), None, "one selection at a time");

    // Select in the conversation again: the pane's ends.
    let (from, to) = (world.at(cx, 0, 8), world.at(cx, 0, 12));
    world.drag(cx, from, to);
    assert!(world.selected(cx).is_some());
    assert_eq!(world.pane_selected(cx), None);

    // Selecting in the composer ends the timeline's selection; selecting in the timeline
    // collapses the composer's.
    let input = world.composer_input(cx);
    input.update_in(cx, |input, window, cx| {
        input.focus(window, cx);
        input.insert_text("some words", window, cx);
        input.set_selection(5, 10, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        world.selected(cx),
        None,
        "the composer's selection replaced it"
    );
    let (from, to) = (world.at(cx, 0, 0), world.at(cx, 0, 4));
    world.drag(cx, from, to);
    let (anchor, head) = input.read_with(cx, |input, _| input.selection());
    assert_eq!(
        anchor, head,
        "the composer's selection collapsed to a caret"
    );
}

// ---- Composition ---------------------------------------------------------------------------------------

#[gpui::test]
fn selection_keys_do_nothing_while_an_input_method_is_composing(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let mine = world.message(&world.me, "My own message", 1);
    world.reach(cx, vec![mine, world.message(&world.bob, "Another", 2)]);
    // Open the message editor and start a composition in it.
    let row = cx.debug_bounds("message-row-0").expect("row");
    cx.simulate_mouse_move(row.center(), None, Modifiers::default());
    draw(cx);
    let edit = cx.debug_bounds("edit-message").expect("toolbar");
    cx.simulate_click(edit.center(), Modifiers::default());
    draw(cx);
    let shell = world.shell(cx);
    let editor = shell
        .read_with(cx, |shell, cx| shell.timeline().read(cx).editor_input())
        .expect("the editor is open");
    editor.update_in(cx, |input, window, cx| {
        input.replace_and_mark_text_in_range(None, "かな", None, window, cx)
    });
    assert!(editor.read_with(cx, |input, _| input.is_composing()));

    // Selecting keys act on the editor's text, never on the timeline, while it composes.
    cx.simulate_keystrokes("shift-up");
    cx.simulate_keystrokes("ctrl-a");
    assert_eq!(world.selected(cx), None, "no timeline selection was made");
    assert!(
        editor.read_with(cx, |input, _| input.is_composing()),
        "the composition went on"
    );
    cx.simulate_keystrokes("escape");
    assert_eq!(world.selected(cx), None);
}

// ---- The thread pane ----------------------------------------------------------------------------------

#[gpui::test]
fn text_is_selected_and_copied_in_the_thread_pane_too(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let mut root = world.message(&world.bob, "Should we ship on Friday?", 1);
    root.thread = Some(ThreadSummary {
        reply_count: 2,
        last_reply_seq: EventSeq::new(3).unwrap(),
        last_reply_at: Timestamp::now(),
        recent_repliers: vec![world.zoe.id],
        as_of: EventSeq::new(3).unwrap(),
    });
    let replies = vec![
        Message {
            attachments: vec![],
            thread_root: Some(root.id),
            ..world.message(&world.zoe, "I would wait", 2)
        },
        Message {
            attachments: vec![],
            thread_root: Some(root.id),
            ..world.message(&world.me, "Agreed, Monday then", 3)
        },
    ];
    world.reach(cx, vec![root.clone()]);
    world.dispatch(
        cx,
        Input::OpenThread {
            conversation: world.general,
            root: root.id,
        },
    );
    let generation = world
        .effects
        .0
        .borrow()
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::FetchReplies { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("asks for replies");
    world.dispatch(
        cx,
        Input::RepliesLoaded {
            root: root.id,
            generation,
            result: Ok(HistoryPage {
                messages: replies.clone(),
                has_more: false,
            }),
        },
    );
    draw(cx);

    // Row 0 is the root, then the two replies. Drag from inside the root into the first reply.
    let (from, to) = (world.pane_at(cx, 0, 7), world.pane_at(cx, 1, 5));
    world.drag(cx, from, to);
    let expected = format!(
        "Bob  {}\nwe ship on Friday?\n\nZoe  {}\nI wou",
        time_of(&root),
        time_of(&replies[0])
    );
    assert_eq!(world.pane_selected(cx), Some(expected.clone()));
    cx.simulate_keystrokes("ctrl-c");
    assert_eq!(clipboard(cx), Some(expected));
    // Ctrl+A in the pane selects the root and every reply, and only those.
    cx.simulate_keystrokes("ctrl-a");
    let all = world.pane_selected(cx).expect("all");
    assert!(all.contains("Should we ship on Friday?") && all.ends_with("Agreed, Monday then"));
    assert_eq!(world.selected(cx), None, "the conversation behind has none");
}
