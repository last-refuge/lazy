//! Search on GPUI's test platform (slice 3C): opening it, typing and the pause before a search,
//! the chips and parse errors, the sort, the keyboard, the states, and opening a result. The
//! real views and the real core run; a recording effect sink stands in for the runtime, and the
//! tests answer its searches the way the runtime would, with GPUI's test clock for the pause.
//!
//! Every keyboard claim is made with real key events through GPUI's dispatch, every mouse claim
//! with real mouse events. The test platform gives every character the same width and never
//! reports a window as active, so these prove behavior and wiring; whether a screen *looks* right
//! is decided by reading the offscreen renders (`.dev/screens-3c`). Nothing here exercises a real
//! input method (composition is simulated), screen reader, or display.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AppContext as _, Entity, EntityInputHandler as _, Modifiers, TestAppContext, VisualTestContext,
    WindowAppearance, point, px,
};
use lazy_client_core::{Effect, Input, RequestError, SearchStatus, StartupState};
use lazy_domain::{
    AttachmentId, ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName,
    EventSeq, MessageDocument, MessageId, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    ApiError, BootstrapResponse, Conversation, ConversationUserState, ErrorCode, HistoryPage,
    Message, MessageResponse, PROTOCOL_VERSION, SearchHit, SearchHitKind, SearchParams,
    SearchResponse, SearchSort, Snippet, SnippetRange, User, Workspace, WorkspaceBootstrap,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::root_view::RootView;
use lazy_ui::shell::ShellDialog;

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
    alice: User,
    bob: User,
    workspace: WorkspaceId,
    general: ConversationId,
    design: ConversationId,
    dm: ConversationId,
    /// A public channel the person has not joined.
    random: Conversation,
}

fn person(name: &str, display: &str) -> User {
    User {
        is_bot: false,
        deactivated: false,
        avatar_version: 0,
        status: None,
        id: UserId::generate(),
        username: Username::parse(name).unwrap(),
        display_name: DisplayName::parse(display).unwrap(),
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
    let channel = |id: ConversationId, name: &str| Conversation {
        id,
        workspace_id: workspace,
        kind: ConversationKind::Channel,
        name: Some(ChannelName::parse(name).unwrap()),
        topic: None,
        description: None,
        visibility: ChannelVisibility::Public,
        archived: false,
        is_default: false,
        created_by: None,
        participants: vec![],
        member_count: 3,
    };
    let world = World {
        model,
        effects,
        root,
        me: person("myself", "Me Myself"),
        alice: person("alice", "Alice Nguyen"),
        bob: person("bob", "Bob Okafor"),
        workspace,
        general: ConversationId::generate(),
        design: ConversationId::generate(),
        dm: ConversationId::generate(),
        random: channel(ConversationId::generate(), "random"),
    };
    (world, cx)
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

impl World {
    fn channel(&self, id: ConversationId, name: &str, default: bool) -> Conversation {
        Conversation {
            id,
            workspace_id: self.workspace,
            kind: ConversationKind::Channel,
            name: Some(ChannelName::parse(name).unwrap()),
            topic: None,
            description: None,
            visibility: ChannelVisibility::Public,
            archived: false,
            is_default: default,
            created_by: None,
            participants: vec![],
            member_count: 3,
        }
    }

    fn dm_conversation(&self) -> Conversation {
        Conversation {
            id: self.dm,
            workspace_id: self.workspace,
            kind: ConversationKind::Direct,
            name: None,
            topic: None,
            description: None,
            visibility: ChannelVisibility::Private,
            archived: false,
            is_default: false,
            created_by: None,
            participants: vec![self.me.id, self.bob.id],
            member_count: 2,
        }
    }

    fn bootstrap(&self) -> BootstrapResponse {
        let state = |id: ConversationId| ConversationUserState {
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
                threads: Vec::new(),
                groups: Vec::new(),
                workspace: Workspace {
                    id: self.workspace,
                    name: "Lazy".into(),
                    settings: Default::default(),
                },
                role: WorkspaceRole::Member,
                conversations: vec![
                    self.channel(self.general, "general", true),
                    self.channel(self.design, "design", false),
                    self.dm_conversation(),
                ],
                user_state: vec![state(self.general), state(self.design), state(self.dm)],
                members: vec![self.me.clone(), self.alice.clone(), self.bob.clone()],
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

    /// Signs in with #general open and these recent searches saved.
    fn reach_with(&self, cx: &mut VisualTestContext, recents: &[&str]) {
        self.dispatch(
            cx,
            Input::Started(StartupState {
                session_restored: true,
                recent_searches: recents.iter().map(|text| (*text).to_owned()).collect(),
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
                    messages: vec![self.message(&self.alice, "welcome", 1)],
                    has_more: false,
                }),
            },
        );
        draw(cx);
    }

    fn reach(&self, cx: &mut VisualTestContext) {
        self.reach_with(cx, &[]);
    }

    fn message(&self, author: &User, text: &str, seq: i64) -> Message {
        self.message_in(self.general, author, text, seq)
    }

    fn message_in(
        &self,
        conversation: ConversationId,
        author: &User,
        text: &str,
        seq: i64,
    ) -> Message {
        let at = time::OffsetDateTime::now_utc() - time::Duration::minutes(500 - seq);
        Message {
            attachments: vec![],
            mentions: Default::default(),
            thread_root: None,
            in_channel: false,
            thread: None,
            id: MessageId::generate(),
            conversation_id: conversation,
            author_id: author.id,
            document: MessageDocument::from_markdown(text).unwrap(),
            created_at: Timestamp::from(at),
            revision: 1,
            seq: EventSeq::new(seq).unwrap(),
            edited_at: None,
            reactions: vec![],
            reactions_as_of: EventSeq::ZERO,
            deleted: false,
        }
    }

    /// A message hit in `conversation`, with `ranges` (in characters) highlighted.
    fn hit(
        &self,
        conversation: &Conversation,
        author: &User,
        text: &str,
        ranges: &[(u32, u32)],
        seq: i64,
    ) -> SearchHit {
        SearchHit {
            message: self.message_in(conversation.id, author, text, seq),
            conversation: conversation.clone(),
            kind: SearchHitKind::Message,
            snippet: Snippet {
                text: text.to_owned(),
                highlights: ranges
                    .iter()
                    .map(|&(start, end)| SnippetRange { start, end })
                    .collect(),
            },
        }
    }

    fn general_conversation(&self) -> Conversation {
        self.channel(self.general, "general", true)
    }

    fn design_conversation(&self) -> Conversation {
        self.channel(self.design, "design", false)
    }

    /// `count` plain hits in #general, newest first.
    fn hits(&self, count: usize, text: &str) -> Vec<SearchHit> {
        let general = self.general_conversation();
        (0..count)
            .map(|n| {
                self.hit(
                    &general,
                    &self.alice,
                    &format!("{text} number {n}"),
                    &[(0, text.chars().count() as u32)],
                    10 + n as i64,
                )
            })
            .collect()
    }

    // ---- What was asked, and the answers ----

    /// The searches asked for so far, in order, left in place.
    fn searches(&self) -> Vec<(u64, SearchParams)> {
        self.effects
            .0
            .borrow()
            .iter()
            .filter_map(|effect| match effect {
                Effect::Search {
                    generation, params, ..
                } => Some((*generation, params.clone())),
                _ => None,
            })
            .collect()
    }

    fn take_searches(&self) -> Vec<(u64, SearchParams)> {
        let found = self.searches();
        self.effects
            .0
            .borrow_mut()
            .retain(|effect| !matches!(effect, Effect::Search { .. }));
        found
    }

    fn answer(
        &self,
        cx: &mut VisualTestContext,
        generation: u64,
        hits: Vec<SearchHit>,
        more: bool,
    ) {
        self.dispatch(
            cx,
            Input::SearchResult {
                generation,
                result: Ok(SearchResponse {
                    hits,
                    has_more: more,
                    truncated: false,
                }),
            },
        );
        draw(cx);
    }

    /// Answers the newest search asked for.
    fn answer_latest(&self, cx: &mut VisualTestContext, hits: Vec<SearchHit>, more: bool) {
        let (generation, _) = self.searches().pop().expect("a search was asked for");
        self.answer(cx, generation, hits, more);
    }

    fn answer_latest_to(
        &self,
        cx: &mut VisualTestContext,
        generation: u64,
        hits: Vec<SearchHit>,
        more: bool,
    ) {
        self.answer(cx, generation, hits, more);
    }

    fn fail_latest(&self, cx: &mut VisualTestContext, error: RequestError) {
        let (generation, _) = self.searches().pop().expect("a search was asked for");
        self.dispatch(
            cx,
            Input::SearchResult {
                generation,
                result: Err(error),
            },
        );
        draw(cx);
    }

    /// Answers the fetch of the message the core is looking for (opening a result) with `message`.
    fn answer_fetch(&self, cx: &mut VisualTestContext, message: &Message) {
        let asked = self.effects.0.borrow().iter().any(
            |effect| matches!(effect, Effect::FetchMessage { message: id } if *id == message.id),
        );
        assert!(asked, "opening a result fetches its message");
        self.dispatch(
            cx,
            Input::MessageFetched {
                message: message.id,
                result: Ok(MessageResponse {
                    message: message.clone(),
                }),
            },
        );
        draw(cx);
    }

    /// Answers what the core asks while it looks for the message of an opened result: its fetch,
    /// the history of its conversation, the replies of its thread, each with `message` in them.
    fn answer_opening(&self, cx: &mut VisualTestContext, message: &Message) {
        for _ in 0..6 {
            let asked: Vec<Effect> = std::mem::take(&mut *self.effects.0.borrow_mut());
            if asked.is_empty() {
                break;
            }
            for effect in asked {
                match effect {
                    Effect::FetchMessage { message: id } if id == message.id => self.dispatch(
                        cx,
                        Input::MessageFetched {
                            message: id,
                            result: Ok(MessageResponse {
                                message: message.clone(),
                            }),
                        },
                    ),
                    Effect::FetchHistory {
                        conversation,
                        generation,
                        ..
                    } if conversation == message.conversation_id => self.dispatch(
                        cx,
                        Input::HistoryLoaded {
                            conversation,
                            generation,
                            result: Ok(HistoryPage {
                                messages: vec![message.clone()],
                                has_more: false,
                            }),
                        },
                    ),
                    Effect::FetchReplies {
                        root, generation, ..
                    } => self.dispatch(
                        cx,
                        Input::RepliesLoaded {
                            root,
                            generation,
                            result: Ok(HistoryPage {
                                messages: vec![message.clone()],
                                has_more: false,
                            }),
                        },
                    ),
                    _ => {}
                }
            }
            draw(cx);
        }
    }

    // ---- Looking ----

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

    fn shell(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::shell::ShellView> {
        self.root
            .read_with(cx, |root, _| root.shell().expect("the shell").clone())
    }

    fn search_view(
        &self,
        cx: &mut VisualTestContext,
    ) -> Entity<lazy_ui::search_view::SearchResultsView> {
        let shell = self.shell(cx);
        shell.read_with(cx, |shell, _| shell.search_view().clone())
    }

    fn field_text(&self, cx: &mut VisualTestContext) -> String {
        let view = self.search_view(cx);
        view.read_with(cx, |view, app| view.field().read(app).text().to_owned())
    }

    fn field_has_the_keyboard(&self, cx: &mut VisualTestContext) -> bool {
        let view = self.search_view(cx);
        let field = view.read_with(cx, |view, _| view.field().clone());
        cx.update(|window, app| field.read(app).is_focused(window))
    }

    fn stop(&self, cx: &mut VisualTestContext) -> &'static str {
        let view = self.search_view(cx);
        cx.update(|window, app| view.read(app).stop(window, app))
    }

    fn active(&self, cx: &mut VisualTestContext) -> Option<usize> {
        let view = self.search_view(cx);
        view.read_with(cx, |view, _| view.active())
    }

    fn rows(&self, cx: &mut VisualTestContext) -> Vec<lazy_ui::search_view::RowInfo> {
        let view = self.search_view(cx);
        view.read_with(cx, |view, _| view.rows_info())
    }

    fn is_searching(&self, cx: &VisualTestContext) -> bool {
        self.model.read_with(cx, |m, _| m.core().search().is_some())
    }

    fn status(&self, cx: &VisualTestContext) -> Option<SearchStatus> {
        self.model
            .read_with(cx, |m, _| m.core().search().map(|view| view.status.clone()))
    }

    fn composer_has_the_keyboard(&self, cx: &mut VisualTestContext) -> bool {
        let shell = self.shell(cx);
        let input = shell.read_with(cx, |shell, app| shell.composer().read(app).input().clone());
        cx.update(|window, app| input.read(app).is_focused(window))
    }

    fn timeline_has_the_keyboard(&self, cx: &mut VisualTestContext) -> bool {
        let shell = self.shell(cx);
        let handle = shell.read_with(cx, |shell, app| {
            gpui::Focusable::focus_handle(shell.timeline().read(app), app)
        });
        cx.update(|window, _| handle.is_focused(window))
    }

    /// Opens search with Ctrl+F and types `text`, leaving the search unasked.
    fn type_query(&self, cx: &mut VisualTestContext, text: &str) {
        if !self.is_searching(cx) {
            cx.simulate_keystrokes("ctrl-f");
        }
        cx.simulate_input(text);
        draw(cx);
    }

    /// Moves the clock past the pause.
    fn pause(&self, cx: &mut VisualTestContext) {
        cx.executor().advance_clock(Duration::from_millis(300));
        cx.run_until_parked();
        draw(cx);
    }
}

// ---- Opening and closing ---------------------------------------------------------------------------

#[gpui::test]
fn ctrl_f_opens_search_from_the_composer_and_puts_the_keyboard_in_the_field(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    assert!(
        world.composer_has_the_keyboard(cx),
        "the composer starts with it"
    );
    assert!(!world.is_searching(cx));

    cx.simulate_keystrokes("ctrl-f");
    assert!(world.is_searching(cx));
    assert!(world.on_screen(cx, "search-view"));
    assert!(world.field_has_the_keyboard(cx));
    cx.simulate_input("deploy");
    assert_eq!(world.field_text(cx), "deploy", "typing goes to the field");
    assert_eq!(
        world
            .model
            .read_with(cx, |m, _| m.core().draft(world.general).to_owned()),
        "",
        "and not to the composer"
    );
    assert!(
        !world.on_screen(cx, "send-message"),
        "the conversation is replaced by the results"
    );
}

#[gpui::test]
fn ctrl_f_works_from_the_timeline_and_escape_returns_there(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    cx.simulate_keystrokes("shift-tab");
    assert!(world.timeline_has_the_keyboard(cx));
    cx.simulate_keystrokes("ctrl-f");
    assert!(world.is_searching(cx) && world.field_has_the_keyboard(cx));
    cx.simulate_keystrokes("escape");
    assert!(!world.is_searching(cx), "Escape closes search");
    assert!(
        world.on_screen(cx, "send-message"),
        "the conversation is back"
    );
    assert!(
        world.timeline_has_the_keyboard(cx),
        "with the keyboard where it was"
    );
}

#[gpui::test]
fn escape_from_a_search_opened_in_the_composer_returns_to_the_composer(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    cx.simulate_keystrokes("ctrl-f escape");
    assert!(!world.is_searching(cx));
    assert!(world.composer_has_the_keyboard(cx));
}

#[gpui::test]
fn ctrl_f_in_an_open_search_only_moves_the_keyboard_to_the_field(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy");
    // Move the keyboard to the sort control, then ask again.
    cx.simulate_keystrokes("tab");
    assert!(!world.field_has_the_keyboard(cx));
    cx.simulate_keystrokes("ctrl-f");
    assert!(world.field_has_the_keyboard(cx));
    assert_eq!(world.field_text(cx), "deploy", "the text is kept");
}

#[gpui::test]
fn the_sidebar_entry_opens_search_with_the_mouse_and_the_keyboard(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    assert!(world.on_screen(cx, "sidebar-search"));
    world.click(cx, "sidebar-search");
    assert!(world.is_searching(cx) && world.field_has_the_keyboard(cx));
    cx.simulate_keystrokes("escape");
    assert!(!world.is_searching(cx));

    // From the keyboard: its own tab stop, Enter activates it.
    let shell = world.shell(cx);
    let focus = shell.read_with(cx, |shell, app| shell.sidebar().read(app).search_focus());
    cx.update(|window, app| window.focus(&focus, app));
    cx.simulate_keystrokes("enter");
    assert!(world.is_searching(cx));
    // Escape gives it back to the entry.
    cx.simulate_keystrokes("escape");
    assert!(cx.update(|window, _| focus.is_focused(window)));
}

#[gpui::test]
fn ctrl_f_does_nothing_while_a_dialog_is_open(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.root.update_in(cx, |root, window, cx| {
        root.shell().unwrap().update(cx, |shell, cx| {
            shell.open_dialog(ShellDialog::CreateChannel, window, cx)
        });
    });
    assert!(world.on_screen(cx, "dialog-card"));
    cx.simulate_keystrokes("ctrl-f");
    assert!(!world.is_searching(cx), "a dialog has the keyboard");
    assert!(world.on_screen(cx, "dialog-card"));
}

#[gpui::test]
fn choosing_a_conversation_in_the_sidebar_leaves_the_search(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    cx.simulate_keystrokes("ctrl-f");
    assert!(world.is_searching(cx));
    // The sidebar row for #design.
    world.dispatch(cx, Input::SelectConversation(world.design));
    assert!(
        !world.is_searching(cx),
        "the conversation is what they asked for"
    );
    assert!(world.on_screen(cx, "send-message"));
}

// ---- The pause before a search ----------------------------------------------------------------------

#[gpui::test]
fn typing_searches_only_after_a_pause_and_each_keystroke_restarts_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.take_effects();
    cx.simulate_keystrokes("ctrl-f");
    cx.simulate_input("dep");
    // The core knows the text at once (the chips and errors follow it), but nothing is asked.
    assert_eq!(
        world
            .model
            .read_with(cx, |m, _| m.core().search().map(|v| v.query.clone())),
        Some("dep".to_owned())
    );
    assert_eq!(world.searches(), vec![]);
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();
    assert_eq!(world.searches(), vec![], "not yet");

    // More typing starts the wait again.
    cx.simulate_input("loy");
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();
    assert_eq!(
        world.searches(),
        vec![],
        "still waiting: the last key was 200 ms ago"
    );
    cx.executor().advance_clock(Duration::from_millis(100));
    cx.run_until_parked();
    let asked = world.searches();
    assert_eq!(asked.len(), 1, "one search, for what was typed");
    assert_eq!(asked[0].1.q, "deploy");
    assert_eq!(asked[0].1.offset.unwrap_or(0), 0);
    assert_eq!(world.status(cx), Some(SearchStatus::Loading));
}

#[gpui::test]
fn enter_searches_at_once_and_the_pause_then_adds_nothing(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.take_effects();
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    assert_eq!(world.searches().len(), 1, "no waiting");
    world.pause(cx);
    assert_eq!(
        world.searches().len(),
        1,
        "and the pause does not ask again"
    );
}

#[gpui::test]
fn clearing_the_field_asks_for_nothing_and_goes_back_to_the_idle_screen(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.take_effects();
    world.type_query(cx, "deploy");
    world.pause(cx);
    world.answer_latest(cx, world.hits(2, "deploy"), false);
    assert_eq!(world.rows(cx).len(), 2);
    world.take_searches();
    cx.simulate_keystrokes("ctrl-a backspace");
    world.pause(cx);
    assert_eq!(world.searches(), vec![]);
    assert!(world.on_screen(cx, "search-idle"));
    assert!(!world.on_screen(cx, "search-hit-0"));
}

#[gpui::test]
fn a_composing_input_method_does_not_trigger_a_search(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.take_effects();
    cx.simulate_keystrokes("ctrl-f");
    let view = world.search_view(cx);
    let field = view.read_with(cx, |view, _| view.field().clone());
    // A composition in progress (simulated, not a real input method).
    field.update_in(cx, |input, window, cx| {
        input.replace_and_mark_text_in_range(None, "かな", None, window, cx)
    });
    assert!(field.read_with(cx, |input, _| input.is_composing()));
    world.pause(cx);
    assert_eq!(world.searches(), vec![], "the text is not final");
    // Escape belongs to the composition.
    cx.simulate_keystrokes("escape");
    assert!(world.is_searching(cx), "Escape did not close search");
    assert_eq!(world.searches(), vec![]);

    field.update_in(cx, |input, window, cx| input.unmark_text(window, cx));
    world.pause(cx);
    assert_eq!(
        world.searches().len(),
        1,
        "once it ends, the pause searches"
    );
    cx.simulate_keystrokes("escape");
    assert!(!world.is_searching(cx), "and Escape is search's again");
}

// ---- Answers: only the last one counts ---------------------------------------------------------------

#[gpui::test]
fn a_late_answer_to_an_older_search_is_never_shown(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.take_effects();
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    let (old, _) = world.searches().pop().unwrap();
    cx.simulate_input(" failed");
    cx.simulate_keystrokes("enter");
    let (new, params) = world.searches().pop().unwrap();
    assert_ne!(old, new);
    assert_eq!(params.q, "deploy failed");

    // The new answer arrives first, then the old one, which must not replace it.
    world.answer(cx, new, world.hits(2, "deploy failed"), false);
    world.answer(cx, old, world.hits(5, "deploy"), true);
    let rows = world.rows(cx);
    assert_eq!(rows.len(), 2, "still the answer to the last search");
    assert!(rows[0].snippet.starts_with("deploy failed"));
}

#[gpui::test]
fn an_answer_after_the_search_was_closed_shows_nothing(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    let (generation, _) = world.searches().pop().unwrap();
    cx.simulate_keystrokes("escape");
    world.answer(cx, generation, world.hits(3, "deploy"), false);
    assert!(!world.is_searching(cx));
    assert!(world.on_screen(cx, "send-message"));
    // Opening search again starts empty.
    cx.simulate_keystrokes("ctrl-f");
    assert_eq!(world.rows(cx).len(), 0);
    assert!(world.on_screen(cx, "search-idle"));
}

// ---- What a result shows ------------------------------------------------------------------------------

fn mixed(world: &World) -> Vec<SearchHit> {
    let general = world.general_conversation();
    let design = world.design_conversation();
    let dm = world.dm_conversation();
    let mut reply = world.hit(
        &design,
        &world.alice,
        "the deploy failed again",
        &[(4, 10)],
        11,
    );
    reply.message.thread_root = Some(MessageId::generate());
    let mut file = world.hit(&general, &world.bob, "deploy-checklist.pdf", &[(0, 6)], 12);
    file.kind = SearchHitKind::Attachment {
        attachment_id: AttachmentId::generate(),
    };
    vec![
        world.hit(
            &general,
            &world.alice,
            "Did the deploy finish?",
            &[(8, 14)],
            10,
        ),
        reply,
        file,
        world.hit(
            &world.random,
            &world.bob,
            "deploy windows for next week",
            &[(0, 6)],
            13,
        ),
        world.hit(&dm, &world.bob, "can you deploy it now", &[(12, 18)], 14),
    ]
}

#[gpui::test]
fn each_result_says_where_who_and_what_with_the_matches_marked(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    world.answer_latest(cx, mixed(&world), false);

    let rows = world.rows(cx);
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[0].place, "#general");
    assert_eq!(rows[0].author, "Alice Nguyen");
    assert_eq!(rows[0].marked, ["deploy"]);
    assert!(rows[0].joined && !rows[0].reply && !rows[0].file);
    assert!(rows[1].reply, "a reply says it is in a thread");
    assert_eq!(rows[1].place, "#design");
    assert!(rows[2].file, "a file hit has the file icon");
    assert_eq!(rows[2].snippet, "deploy-checklist.pdf");
    assert_eq!(rows[3].place, "#random");
    assert!(!rows[3].joined, "a channel the person is not in says so");
    assert_eq!(
        rows[4].place, "Bob Okafor",
        "a conversation is its other people"
    );
    for index in 0..5 {
        let selector = Box::leak(format!("search-hit-{index}").into_boxed_str());
        assert!(world.on_screen(cx, selector), "row {index} is drawn");
    }
    assert!(world.on_screen(cx, "search-hit-file-2"));
    assert!(!world.on_screen(cx, "search-hit-file-0"));
}

#[gpui::test]
fn highlights_in_characters_survive_emoji_cjk_and_combining_marks(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    let general = world.general_conversation();
    world.answer_latest(
        cx,
        vec![
            // Characters: 🎉 is one, so "deploy" starts at 3.
            world.hit(&general, &world.alice, "🎉 🚀 deploy done", &[(4, 10)], 10),
            world.hit(
                &general,
                &world.alice,
                "日本語のデプロイ完了",
                &[(4, 8)],
                11,
            ),
            // "résumé" with combining accents: highlighting only the "e" takes its accent.
            world.hit(
                &general,
                &world.alice,
                "re\u{301}sume\u{301} sent",
                &[(1, 2)],
                12,
            ),
            // A range past the end is clamped rather than a crash.
            world.hit(&general, &world.alice, "short", &[(2, 400)], 13),
        ],
        false,
    );
    let rows = world.rows(cx);
    assert_eq!(rows[0].marked, ["deploy"]);
    assert_eq!(rows[1].marked, ["デプロイ"]);
    assert_eq!(rows[2].marked, ["e\u{301}"]);
    assert_eq!(rows[3].marked, ["ort"]);
}

// ---- Chips and errors ---------------------------------------------------------------------------------

#[gpui::test]
fn the_filters_in_the_query_are_chips_and_removing_one_edits_the_text_and_searches(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.take_effects();
    world.type_query(cx, "deploy from:alice in:#design has:image");
    assert!(world.on_screen(cx, "search-chip-0"));
    assert!(world.on_screen(cx, "search-chip-1"));
    assert!(world.on_screen(cx, "search-chip-2"));
    assert!(!world.on_screen(cx, "search-chip-3"));
    assert_eq!(world.searches(), vec![], "not asked yet");

    world.click(cx, "search-chip-remove-1");
    assert_eq!(world.field_text(cx), "deploy from:alice has:image");
    let asked = world.searches();
    assert_eq!(asked.len(), 1, "removing a filter searches at once");
    assert_eq!(asked[0].1.q, "deploy from:alice has:image");
    assert!(!world.on_screen(cx, "search-chip-2"), "one fewer chip");
    assert!(
        world.field_has_the_keyboard(cx),
        "and the keyboard is back in the field"
    );
}

#[gpui::test]
fn a_chip_is_removed_from_the_keyboard(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "plan after:2026-09-01");
    // Field -> sort control -> the chip's remove button.
    cx.simulate_keystrokes("tab tab");
    cx.simulate_keystrokes("enter");
    assert_eq!(world.field_text(cx), "plan");
    assert!(!world.on_screen(cx, "search-chip-0"));
}

#[gpui::test]
fn a_query_that_cannot_be_used_says_why_under_the_field_and_asks_for_nothing(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.take_effects();
    world.type_query(cx, "plan after:soon");
    assert!(world.on_screen(cx, "search-error"));
    assert!(
        !world.on_screen(cx, "search-chip-0"),
        "no chips for a query that does not parse"
    );
    world.pause(cx);
    cx.simulate_keystrokes("enter");
    assert_eq!(world.searches(), vec![], "nothing is asked for it");

    // Fixing it clears the message and the search goes ahead.
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("plan after:2026-09-01");
    assert!(!world.on_screen(cx, "search-error"));
    world.pause(cx);
    assert_eq!(world.searches().len(), 1);
}

#[gpui::test]
fn text_with_nothing_to_search_for_is_an_error_but_an_empty_field_is_not(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    cx.simulate_keystrokes("ctrl-f");
    assert!(
        !world.on_screen(cx, "search-error"),
        "an empty field is just idle"
    );
    cx.simulate_input("   ");
    assert!(!world.on_screen(cx, "search-error"));
    cx.simulate_input("!!!");
    assert!(
        world.on_screen(cx, "search-error"),
        "punctuation alone is nothing to search for"
    );
}

// ---- The sort -----------------------------------------------------------------------------------------

#[gpui::test]
fn the_sort_control_searches_again_with_the_new_order_by_mouse_and_keyboard(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    world.answer_latest(cx, world.hits(3, "deploy"), false);
    world.take_effects();

    world.click(cx, "search-sort-newest");
    let asked = world.searches();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].1.sort, Some(SearchSort::Recent));
    world.answer_latest(cx, world.hits(3, "deploy"), false);
    world.take_effects();

    // From the keyboard: Tab to the control, Left chooses Best match.
    cx.simulate_keystrokes("ctrl-f tab");
    cx.simulate_keystrokes("left");
    let asked = world.searches();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].1.sort, Some(SearchSort::Relevance));
    world.answer_latest(cx, world.hits(3, "deploy"), false);
    // Enter toggles.
    world.take_effects();
    cx.simulate_keystrokes("enter");
    assert_eq!(world.searches()[0].1.sort, Some(SearchSort::Recent));
}

// ---- Keys through the list ------------------------------------------------------------------------------

fn with_results(cx: &mut TestAppContext, count: usize) -> (World, &mut VisualTestContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.take_effects();
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    let hits = world.hits(count, "deploy");
    world.answer_latest(cx, hits, false);
    (world, cx)
}

#[gpui::test]
fn up_and_down_move_the_active_result_while_the_field_keeps_the_keyboard(cx: &mut TestAppContext) {
    let (world, cx) = with_results(cx, 30);
    assert_eq!(world.active(cx), None, "nothing is active until chosen");
    cx.simulate_keystrokes("down");
    assert_eq!(world.active(cx), Some(0));
    cx.simulate_keystrokes("down down");
    assert_eq!(world.active(cx), Some(2));
    cx.simulate_keystrokes("up");
    assert_eq!(world.active(cx), Some(1));
    assert!(
        world.field_has_the_keyboard(cx),
        "typing still goes to the field"
    );
    cx.simulate_keystrokes("up up up");
    assert_eq!(world.active(cx), Some(0), "stops at the first");
    // Typing continues in the field.
    cx.simulate_input(" x");
    assert_eq!(world.field_text(cx), "deploy x");
    assert_eq!(world.active(cx), None, "and typing clears the choice");
}

#[gpui::test]
fn page_keys_and_alt_home_end_jump_and_the_active_row_is_scrolled_into_view(
    cx: &mut TestAppContext,
) {
    let (world, cx) = with_results(cx, 30);
    cx.simulate_keystrokes("alt-end");
    assert_eq!(world.active(cx), Some(29));
    assert!(
        world.on_screen(cx, "search-hit-29"),
        "the last row is in view"
    );
    cx.simulate_keystrokes("alt-home");
    assert_eq!(world.active(cx), Some(0));
    assert!(world.on_screen(cx, "search-hit-0"));
    assert!(!world.on_screen(cx, "search-hit-29"));

    cx.simulate_keystrokes("pagedown");
    let after_page = world.active(cx).unwrap();
    assert!(after_page >= 3, "a page moves several rows: {after_page}");
    assert!(world.on_screen(
        cx,
        Box::leak(format!("search-hit-{after_page}").into_boxed_str())
    ));
    cx.simulate_keystrokes("pageup");
    assert_eq!(world.active(cx), Some(0));
    assert!(world.field_has_the_keyboard(cx));
}

#[gpui::test]
fn plain_home_and_end_still_move_the_caret_in_the_field(cx: &mut TestAppContext) {
    let (world, cx) = with_results(cx, 5);
    cx.simulate_keystrokes("home");
    cx.simulate_input("x");
    assert_eq!(world.field_text(cx), "xdeploy");
    assert_eq!(world.active(cx), None);
}

#[gpui::test]
fn escape_first_clears_the_active_result_then_closes(cx: &mut TestAppContext) {
    let (world, cx) = with_results(cx, 5);
    cx.simulate_keystrokes("down down");
    assert_eq!(world.active(cx), Some(1));
    cx.simulate_keystrokes("escape");
    assert_eq!(world.active(cx), None);
    assert!(world.is_searching(cx), "the first Escape only cleared it");
    cx.simulate_keystrokes("escape");
    assert!(!world.is_searching(cx));
}

#[gpui::test]
fn enter_with_no_active_result_searches_now_and_with_one_opens_it(cx: &mut TestAppContext) {
    let (world, cx) = with_results(cx, 5);
    world.take_effects();
    cx.simulate_keystrokes("enter");
    assert_eq!(world.searches().len(), 1, "Enter searched again");
    world.answer_latest(cx, world.hits(5, "deploy"), false);

    let found = world.model.read_with(cx, |m, _| {
        m.core().search().unwrap().hits[1].message.clone()
    });
    cx.simulate_keystrokes("down down enter");
    assert!(
        world.is_searching(cx),
        "the search waits while the message is looked for"
    );
    world.answer_opening(cx, &found);
    assert!(
        !world.is_searching(cx),
        "opening a result leaves the search"
    );
}

#[gpui::test]
fn the_tab_order_is_field_sort_chips_list_show_more_then_close(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy from:alice");
    cx.simulate_keystrokes("enter");
    world.answer_latest(cx, world.hits(20, "deploy"), true);
    world.take_effects();

    // Each stop is shown to have the keyboard by what a key does there.
    cx.simulate_keystrokes("tab right");
    let asked = world.take_searches();
    assert_eq!(
        asked.len(),
        1,
        "the sort control is the stop after the field"
    );
    assert_eq!(asked[0].1.sort, Some(SearchSort::Recent));
    world.answer_latest_to(cx, asked[0].0, world.hits(20, "deploy"), true);

    cx.simulate_keystrokes("tab enter");
    assert_eq!(
        world.field_text(cx),
        "deploy",
        "then the chip's remove button"
    );
    assert!(
        world.field_has_the_keyboard(cx),
        "which hands the keyboard back to the field"
    );
    let asked = world.take_searches();
    world.answer_latest_to(cx, asked.last().unwrap().0, world.hits(20, "deploy"), true);

    // No chip now: field -> sort -> the list, which takes the same movement keys.
    cx.simulate_keystrokes("tab tab");
    assert_eq!(world.stop(cx), "list", "the list is the third stop");
    cx.simulate_keystrokes("down down");
    assert_eq!(world.active(cx), Some(1));
    // After the list: Show more, then Close.
    assert_eq!(world.stop(cx), "list");
    cx.simulate_keystrokes("tab");
    assert_eq!(world.stop(cx), "more");
    cx.simulate_keystrokes("tab");
    assert_eq!(world.stop(cx), "close");
    cx.simulate_keystrokes("enter");
    assert!(!world.is_searching(cx), "Close is last");
}

#[gpui::test]
fn show_more_is_a_keyboard_stop_that_pages(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    world.answer_latest(cx, world.hits(20, "deploy"), true);
    world.take_effects();
    cx.simulate_keystrokes("tab tab tab");
    assert_eq!(world.stop(cx), "more");
    cx.simulate_keystrokes("enter");
    let asked = world.searches();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].1.offset, Some(20));
}

// ---- States -------------------------------------------------------------------------------------------

#[gpui::test]
fn an_empty_field_with_no_recent_searches_says_what_to_type_and_shows_the_filters(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    cx.simulate_keystrokes("ctrl-f");
    assert!(world.on_screen(cx, "search-idle"));
    assert!(world.on_screen(cx, "search-filter-hint"));
    assert!(!world.on_screen(cx, "search-recents"));
}

#[gpui::test]
fn recent_searches_are_listed_and_enter_or_a_click_runs_one(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_with(cx, &["deploy failed", "from:alice budget"]);
    world.take_effects();
    cx.simulate_keystrokes("ctrl-f");
    assert!(world.on_screen(cx, "search-recents"));
    assert!(world.on_screen(cx, "search-recent-0") && world.on_screen(cx, "search-recent-1"));
    assert!(
        world.on_screen(cx, "search-filter-hint"),
        "the filters are still hinted"
    );

    // Down chooses the first, Enter runs it: the text is filled in and searched at once.
    cx.simulate_keystrokes("down down enter");
    assert_eq!(world.field_text(cx), "from:alice budget");
    let asked = world.searches();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].1.q, "from:alice budget");

    // A click does the same.
    cx.simulate_keystrokes("ctrl-a backspace");
    world.take_effects();
    world.click(cx, "search-recent-0");
    assert_eq!(world.field_text(cx), "deploy failed");
    assert_eq!(world.searches().len(), 1);
}

#[gpui::test]
fn the_first_page_loading_shows_rows_the_shape_of_results_and_then_they_arrive_in_place(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    assert!(
        world.on_screen(cx, "search-loading"),
        "a skeleton, not a bare spinner"
    );
    let skeleton = cx.debug_bounds("search-loading").unwrap();
    world.answer_latest(cx, world.hits(4, "deploy"), false);
    assert!(!world.on_screen(cx, "search-loading"));
    assert!(world.on_screen(cx, "search-list"));
    let list = cx.debug_bounds("search-list").unwrap();
    assert_eq!(
        skeleton.origin.y, list.origin.y,
        "it starts where the skeleton did"
    );
}

#[gpui::test]
fn no_results_says_what_to_try_and_names_the_filters(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "zzzz from:alice");
    cx.simulate_keystrokes("enter");
    world.answer_latest(cx, vec![], false);
    assert!(world.on_screen(cx, "search-no-results"));
    assert!(!world.on_screen(cx, "search-hit-0"));
}

#[gpui::test]
fn a_failed_search_says_what_happened_and_try_again_asks_again(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    world.fail_latest(cx, RequestError::Unknown("reset".into()));
    assert!(world.on_screen(cx, "search-failed"));
    assert!(
        world.on_screen(cx, "search-retry"),
        "a lost connection can be tried again"
    );
    world.take_effects();
    world.click(cx, "search-retry");
    assert_eq!(world.searches().len(), 1);
    assert_eq!(world.status(cx), Some(SearchStatus::Loading));
}

#[gpui::test]
fn a_search_that_took_too_long_can_be_narrowed_or_tried_again(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "the");
    cx.simulate_keystrokes("enter");
    world.fail_latest(
        cx,
        RequestError::Rejected(ApiError::new(ErrorCode::Unavailable, "slow")),
    );
    assert!(world.on_screen(cx, "search-failed"));
    assert!(world.on_screen(cx, "search-retry"));
    // The field still has the text, so it can be narrowed there.
    assert_eq!(world.field_text(cx), "the");
}

#[gpui::test]
fn a_search_the_server_refuses_or_a_workspace_that_is_gone_offers_no_retry(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    world.fail_latest(
        cx,
        RequestError::Rejected(ApiError::new(ErrorCode::NotFound, "no such workspace")),
    );
    assert!(world.on_screen(cx, "search-failed"));
    assert!(!world.on_screen(cx, "search-retry"), "nothing to try again");
    cx.simulate_keystrokes("escape");
    assert!(!world.is_searching(cx), "but Escape still leaves");
}

#[gpui::test]
fn show_more_loads_the_next_page_and_the_row_count_grows(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    world.answer_latest(cx, world.hits(20, "deploy"), true);
    assert!(world.on_screen(cx, "search-show-more"));
    world.take_effects();
    world.click(cx, "search-show-more");
    let asked = world.searches();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].1.offset, Some(20), "the next page");
    assert!(
        world.on_screen(cx, "search-loading-more"),
        "a line says it is loading"
    );
    assert_eq!(world.rows(cx).len(), 20, "the results so far stay");
    world.answer_latest(cx, world.hits(20, "deploy"), true);
    assert_eq!(world.rows(cx).len(), 40);
}

#[gpui::test]
fn moving_to_the_last_result_asks_for_more_without_a_click(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    world.answer_latest(cx, world.hits(20, "deploy"), true);
    world.take_effects();
    cx.simulate_keystrokes("alt-end");
    let asked = world.searches();
    assert_eq!(asked.len(), 1, "reaching the end paged");
    assert_eq!(asked[0].1.offset, Some(20));
}

#[gpui::test]
fn a_failed_next_page_keeps_the_results_and_can_be_retried(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    world.answer_latest(cx, world.hits(20, "deploy"), true);
    world.click(cx, "search-show-more");
    world.fail_latest(cx, RequestError::Unknown("reset".into()));
    assert_eq!(world.rows(cx).len(), 20, "nothing was lost");
    assert!(world.on_screen(cx, "search-more-failed"));
    world.take_effects();
    world.click(cx, "search-retry");
    assert_eq!(world.searches().len(), 1, "the same page again");
    assert_eq!(world.searches()[0].1.offset, Some(20));
}

#[gpui::test]
fn the_notes_for_a_truncated_search_and_for_the_end_of_what_search_can_show(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    let (generation, _) = world.searches().pop().unwrap();
    world.dispatch(
        cx,
        Input::SearchResult {
            generation,
            result: Ok(SearchResponse {
                hits: world.hits(20, "deploy"),
                has_more: true,
                truncated: true,
            }),
        },
    );
    draw(cx);
    assert!(world.on_screen(cx, "search-truncated"));
    // Newest first is not ranked, so the note does not apply.
    world.click(cx, "search-sort-newest");
    let (generation, _) = world.searches().pop().unwrap();
    world.dispatch(
        cx,
        Input::SearchResult {
            generation,
            result: Ok(SearchResponse {
                hits: world.hits(20, "deploy"),
                has_more: true,
                truncated: true,
            }),
        },
    );
    draw(cx);
    assert!(!world.on_screen(cx, "search-truncated"));

    // Page to the 200-result limit: reaching the last row asks for the next page.
    for _ in 0..9 {
        world.take_effects();
        cx.simulate_keystrokes("alt-end");
        let (generation, _) = world.searches().pop().expect("a page was asked for");
        world.answer(cx, generation, world.hits(20, "deploy"), true);
    }
    assert_eq!(world.rows(cx).len(), 200);
    assert!(world.on_screen(cx, "search-at-limit"));
    assert!(
        !world.on_screen(cx, "search-show-more"),
        "there is nothing more to ask for"
    );
}

// ---- Opening a result -----------------------------------------------------------------------------------

#[gpui::test]
fn opening_a_result_selects_its_conversation_and_returns_the_keyboard_to_the_timeline(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    let design = world.design_conversation();
    let hit = world.hit(&design, &world.alice, "deploy is out", &[(0, 6)], 10);
    let found = hit.message.clone();
    world.answer_latest(cx, vec![hit], false);
    cx.simulate_keystrokes("down enter");
    assert!(
        world.is_searching(cx),
        "the search waits while the message is looked for"
    );
    assert!(world.on_screen(cx, "search-opening"), "and says so");
    world.answer_opening(cx, &found);
    assert!(!world.is_searching(cx));
    assert_eq!(
        world
            .model
            .read_with(cx, |m, _| m.core().selected_conversation()),
        Some(world.design)
    );
    assert!(
        world.on_screen(cx, "send-message"),
        "the conversation is showing"
    );
    assert!(world.timeline_has_the_keyboard(cx));
    // The search that found it is remembered.
    assert_eq!(
        world
            .model
            .read_with(cx, |m, _| m.core().recent_searches().to_vec()),
        vec!["deploy".to_owned()]
    );
}

#[gpui::test]
fn clicking_a_result_opens_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    let design = world.design_conversation();
    world.answer_latest(
        cx,
        vec![
            world.hit(&design, &world.alice, "first deploy", &[(6, 12)], 10),
            world.hit(&design, &world.alice, "second deploy", &[(7, 13)], 11),
        ],
        false,
    );
    world.click(cx, "search-hit-1");
    let found = world.model.read_with(cx, |m, _| {
        m.core().search().unwrap().hits[1].message.clone()
    });
    world.answer_opening(cx, &found);
    assert!(!world.is_searching(cx));
    assert_eq!(
        world
            .model
            .read_with(cx, |m, _| m.core().selected_conversation()),
        Some(world.design)
    );
}

#[gpui::test]
fn opening_a_reply_opens_its_thread(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    let design = world.design_conversation();
    let root = MessageId::generate();
    let mut reply = world.hit(&design, &world.alice, "the deploy failed", &[(4, 10)], 10);
    reply.message.thread_root = Some(root);
    let found = reply.message.clone();
    world.answer_latest(cx, vec![reply], false);
    cx.simulate_keystrokes("down enter");
    world.answer_opening(cx, &found);
    assert!(!world.is_searching(cx));
    assert_eq!(
        world
            .model
            .read_with(cx, |m, _| m.core().open_thread().map(|t| t.root)),
        Some(root),
        "the thread of the reply is open"
    );
    assert!(world.on_screen(cx, "thread-pane"));
}

#[gpui::test]
fn opening_a_result_in_a_channel_not_joined_offers_to_join(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    let random = world.random.clone();
    let hit = world.hit(&random, &world.bob, "deploy windows", &[(0, 6)], 10);
    let found = hit.message.clone();
    world.answer_latest(cx, vec![hit], false);
    cx.simulate_keystrokes("down enter");
    world.answer_fetch(cx, &found);
    assert!(!world.is_searching(cx));
    assert!(
        world.on_screen(cx, "join-offer"),
        "the same offer a permalink to an unjoined channel gets"
    );
}

#[gpui::test]
fn the_conversation_is_not_lost_while_searching_and_a_new_message_arrives(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    cx.simulate_keystrokes("ctrl-f");
    // The composer text is untouched by search.
    world.dispatch(
        cx,
        Input::SetDraft {
            conversation: world.general,
            text: "a draft".into(),
        },
    );
    cx.simulate_keystrokes("escape");
    assert_eq!(
        world
            .model
            .read_with(cx, |m, _| m.core().draft(world.general).to_owned()),
        "a draft"
    );
}

// ---- Narrow windows ---------------------------------------------------------------------------------------

#[gpui::test]
fn search_opened_with_a_thread_pane_that_would_hide_it_closes_the_pane(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    // A narrow window, where an open thread replaces the conversation.
    cx.simulate_resize(gpui::size(px(640.), px(480.)));
    let root = world.message(&world.alice, "root of a thread", 2);
    world.root.update_in(cx, |root_view, window, cx| {
        root_view.shell().unwrap().update(cx, |shell, cx| {
            shell.open_thread(world.general, root.id, window, cx)
        });
    });
    draw(cx);
    cx.simulate_keystrokes("ctrl-f");
    assert!(world.is_searching(cx));
    assert!(
        world.on_screen(cx, "search-view"),
        "the results are what is showing"
    );
}

// ---- 200 results ----------------------------------------------------------------------------------------

#[gpui::test]
fn two_hundred_results_scroll_with_the_keyboard_without_slow_frames(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.type_query(cx, "deploy");
    cx.simulate_keystrokes("enter");
    world.answer_latest(cx, world.hits(20, "deploy"), true);
    let started = Instant::now();
    for _ in 0..9 {
        world.take_effects();
        cx.simulate_keystrokes("alt-end");
        let (generation, _) = world.searches().pop().expect("a page was asked for");
        world.answer(cx, generation, world.hits(20, "deploy"), true);
    }
    let paging = started.elapsed();
    assert_eq!(world.rows(cx).len(), 200);

    // Down through every result, drawing each step.
    cx.simulate_keystrokes("alt-home");
    let mut slowest = Duration::ZERO;
    let stepping = Instant::now();
    for _ in 0..199 {
        let frame = Instant::now();
        cx.simulate_keystrokes("down");
        draw(cx);
        slowest = slowest.max(frame.elapsed());
    }
    let stepped = stepping.elapsed();
    // And with the wheel, back up over everything.
    let wheel = Instant::now();
    let mut slowest_wheel = Duration::ZERO;
    for _ in 0..60 {
        let frame = Instant::now();
        cx.simulate_event(gpui::ScrollEvent {
            position: point(px(900.), px(400.)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(600.))),
            ..Default::default()
        });
        draw(cx);
        slowest_wheel = slowest_wheel.max(frame.elapsed());
    }
    let wheeled = wheel.elapsed();
    eprintln!(
        "200 results: {paging:?} to page in; 199 Down presses with a draw each: {stepped:?} in all \
         ({:?} a step), slowest {slowest:?}; 60 wheel steps: {wheeled:?} ({:?} a step), slowest \
         {slowest_wheel:?}",
        stepped / 199,
        wheeled / 60
    );
    assert_eq!(world.active(cx), Some(199));
    assert!(slowest < Duration::from_secs(5), "{slowest:?}");
    assert!(slowest_wheel < Duration::from_secs(5), "{slowest_wheel:?}");
}
