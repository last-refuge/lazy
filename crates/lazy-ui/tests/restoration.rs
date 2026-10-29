//! The caret and selection of the composer and the thread reply box are kept with their drafts:
//! reported to the core as they move, put back when a conversation or thread is shown again, and
//! restored from what the core loaded at startup. Real key events, the real core, a recording sink.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, Modifiers, TestAppContext, VisualTestContext, WindowAppearance, px,
    size,
};
use lazy_client_core::{
    DraftSelection, Effect, Input, StartupState, TextSelection, ThreadDraft, ViewState,
};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    MessageDocument, MessageId, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    BootstrapResponse, Conversation, ConversationUserState, HistoryPage, Message, PROTOCOL_VERSION,
    ThreadSummary, User, Workspace, WorkspaceBootstrap,
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
    root: Entity<RootView>,
    me: User,
    bob: User,
    workspace: WorkspaceId,
    general: ConversationId,
    design: ConversationId,
}

fn person(name: &str) -> User {
    User {
        id: UserId::generate(),
        username: Username::parse(name).unwrap(),
        display_name: DisplayName::parse(&format!("User {name}")).unwrap(),
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
    (
        World {
            model,
            effects,
            root,
            me: person("alice"),
            bob: person("bob"),
            workspace: WorkspaceId::generate(),
            general: ConversationId::generate(),
            design: ConversationId::generate(),
        },
        cx,
    )
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

impl World {
    fn channel(&self, id: ConversationId, name: &str) -> Conversation {
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
            member_count: 2,
        }
    }

    fn bootstrap(&self) -> BootstrapResponse {
        let state = |id| ConversationUserState {
            conversation_id: id,
            favorite: false,
            last_read: EventSeq::ZERO,
            unread_count: 0,
            mention_count: 0,
        };
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
                conversations: vec![
                    self.channel(self.general, "general"),
                    self.channel(self.design, "design"),
                ],
                user_state: vec![state(self.general), state(self.design)],
                members: vec![self.me.clone(), self.bob.clone()],
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

    /// Starts the app as if from `startup`, and shows `history` in #general.
    fn reach(&self, cx: &mut VisualTestContext, startup: StartupState, history: Vec<Message>) {
        self.dispatch(
            cx,
            Input::Started(StartupState {
                session_restored: true,
                ..startup
            }),
        );
        self.dispatch(cx, Input::BootstrapLoaded(Box::new(self.bootstrap())));
        let selected = self
            .model
            .read_with(cx, |m, _| m.core().selected_conversation())
            .expect("a conversation is selected");
        let generation = self
            .take_effects()
            .into_iter()
            .find_map(|effect| match effect {
                Effect::FetchHistory {
                    conversation,
                    generation,
                    ..
                } if conversation == selected => Some(generation),
                _ => None,
            })
            .expect("the selected conversation fetches its history");
        self.dispatch(
            cx,
            Input::HistoryLoaded {
                conversation: selected,
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

    fn click(&self, cx: &mut VisualTestContext, selector: &'static str) {
        draw(cx);
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("nothing is labelled {selector:?} on screen"));
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();
    }

    fn shell(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::shell::ShellView> {
        self.root.read_with(cx, |root, _| {
            root.shell().expect("the shell is showing").clone()
        })
    }

    fn composer_input(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::text_input::TextInput> {
        let shell = self.shell(cx);
        shell.read_with(cx, |shell, cx| shell.composer().read(cx).input().clone())
    }

    fn reply_input(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::text_input::TextInput> {
        let shell = self.shell(cx);
        shell.read_with(cx, |shell, cx| {
            shell
                .thread_pane()
                .read(cx)
                .composer()
                .read(cx)
                .input()
                .clone()
        })
    }

    fn input_state(
        &self,
        cx: &mut VisualTestContext,
        input: &Entity<lazy_ui::text_input::TextInput>,
    ) -> (String, (usize, usize)) {
        draw(cx);
        input.read_with(cx, |input, _| (input.text().to_owned(), input.selection()))
    }

    fn saved_selection(
        &self,
        cx: &VisualTestContext,
        conversation: ConversationId,
    ) -> Option<(usize, usize)> {
        self.model.read_with(cx, |m, _| {
            m.core()
                .draft_selection(conversation)
                .map(|s| (s.anchor, s.head))
        })
    }
}

/// Presses the keys that put the selection over "brave" in "hello brave world".
const SELECT_BRAVE: &str = "home right right right right right right shift-right shift-right shift-right shift-right shift-right";

#[gpui::test]
fn the_composer_reports_where_the_caret_and_selection_are(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, StartupState::default(), vec![]);
    cx.simulate_input("hello brave world");
    assert_eq!(
        world.saved_selection(cx, world.general),
        Some((17, 17)),
        "the caret is at the end"
    );
    cx.simulate_keystrokes(SELECT_BRAVE);
    assert_eq!(world.saved_selection(cx, world.general), Some((6, 11)));
    cx.simulate_keystrokes("left");
    assert_eq!(
        world.saved_selection(cx, world.general),
        Some((6, 6)),
        "a caret again"
    );
}

#[gpui::test]
fn switching_away_and_back_puts_the_caret_and_selection_back(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, StartupState::default(), vec![]);
    cx.simulate_input("hello brave world");
    cx.simulate_keystrokes(SELECT_BRAVE);
    let input = world.composer_input(cx);
    assert_eq!(world.input_state(cx, &input).1, (6, 11));

    // Another conversation has its own (empty) draft and caret.
    world.click(cx, "conv-design");
    let (text, selection) = world.input_state(cx, &input);
    assert_eq!((text.as_str(), selection), ("", (0, 0)));
    cx.simulate_input("other");

    // Back: the text, and "brave" selected as it was left.
    world.click(cx, "conv-general");
    let (text, selection) = world.input_state(cx, &input);
    assert_eq!(text, "hello brave world");
    assert_eq!(selection, (6, 11), "the selection was restored");
    // Typing replaces the restored selection, as it would have.
    cx.simulate_input("lovely");
    assert_eq!(world.input_state(cx, &input).0, "hello lovely world");

    // And the design draft kept its own caret at its end.
    world.click(cx, "conv-design");
    assert_eq!(world.input_state(cx, &input), ("other".to_owned(), (5, 5)));
}

#[gpui::test]
fn restoring_a_position_is_not_reported_back_as_a_move(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, StartupState::default(), vec![]);
    cx.simulate_input("hello brave world");
    cx.simulate_keystrokes(SELECT_BRAVE);
    world.click(cx, "conv-design");
    world.take_effects();
    world.click(cx, "conv-general");
    // Showing the conversation again set the selection; the core already had it.
    assert_eq!(world.saved_selection(cx, world.general), Some((6, 11)));
    let input = world.composer_input(cx);
    assert_eq!(world.input_state(cx, &input).1, (6, 11));
}

#[gpui::test]
fn a_restarted_app_restores_the_caret_from_the_drafts_the_core_loaded(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let startup = StartupState {
        drafts: [(world.general, "hello brave world".to_owned())].into(),
        view: ViewState {
            last_selected: vec![(world.workspace, world.general)],
            draft_selections: vec![DraftSelection {
                conversation: world.general,
                selection: TextSelection {
                    anchor: 6,
                    head: 11,
                },
            }],
        },
        ..Default::default()
    };
    world.reach(cx, startup, vec![]);
    let input = world.composer_input(cx);
    let (text, selection) = world.input_state(cx, &input);
    assert_eq!(text, "hello brave world");
    assert_eq!(
        selection,
        (6, 11),
        "the selection a previous run saved is back"
    );
}

#[gpui::test]
fn the_thread_reply_box_keeps_its_caret_too_and_a_restart_restores_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let mut root = world.message(&world.bob, "Root of a thread", 1);
    root.thread = Some(ThreadSummary {
        reply_count: 1,
        last_reply_seq: EventSeq::new(2).unwrap(),
        last_reply_at: Timestamp::now(),
        recent_repliers: vec![world.bob.id],
        as_of: EventSeq::new(2).unwrap(),
    });
    let reply = Message {
        attachments: vec![],
        thread_root: Some(root.id),
        ..world.message(&world.bob, "A reply", 2)
    };
    // As if a previous run left a reply draft with "brave" selected.
    let startup = StartupState {
        thread_drafts: vec![ThreadDraft {
            root: root.id,
            conversation: world.general,
            text: "hello brave world".to_owned(),
            selection: Some(TextSelection {
                anchor: 6,
                head: 11,
            }),
        }],
        ..Default::default()
    };
    world.reach(cx, startup, vec![root.clone()]);
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
        .expect("opening the thread asks for its replies");
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
    let input = world.reply_input(cx);
    let (text, selection) = world.input_state(cx, &input);
    assert_eq!(text, "hello brave world", "the saved reply is in the box");
    assert_eq!(selection, (6, 11), "with its selection");

    // Moving the caret is reported for the thread, and survives closing and opening it again.
    cx.simulate_keystrokes("left");
    let saved = world.model.read_with(cx, |m, _| {
        m.core()
            .thread_draft_selection(root.id)
            .map(|s| (s.anchor, s.head))
    });
    assert_eq!(saved, Some((6, 6)));
    world.dispatch(cx, Input::CloseThread);
    world.dispatch(
        cx,
        Input::OpenThread {
            conversation: world.general,
            root: root.id,
        },
    );
    let (text, selection) = world.input_state(cx, &input);
    assert_eq!((text.as_str(), selection), ("hello brave world", (6, 6)));
}

#[gpui::test]
fn a_restarted_app_reopens_on_the_conversation_that_was_last_open(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let startup = StartupState {
        view: ViewState {
            last_selected: vec![(world.workspace, world.design)],
            draft_selections: vec![],
        },
        ..Default::default()
    };
    world.reach(cx, startup, vec![]);
    let selected = world
        .model
        .read_with(cx, |m, _| m.core().selected_conversation());
    assert_eq!(selected, Some(world.design), "not the default channel");
    draw(cx);
    assert!(cx.debug_bounds("conv-design").is_some());
    // The composer is for that conversation: what is typed becomes its draft.
    cx.simulate_input("hi");
    let draft = world
        .model
        .read_with(cx, |m, _| m.core().draft(world.design).to_owned());
    assert_eq!(draft, "hi");
}
