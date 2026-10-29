//! UI-level flows on GPUI's test platform: the real views, the real core, and a recording
//! effect sink in place of the runtime.
//!
//! These prove that keystrokes reach the core as the right inputs, that the core's effects come
//! out the other side, and that every screen renders without panicking. They do not prove real
//! glyph layout, IME, or what appears on a physical display (the test platform's text system
//! gives every character the same width).

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, Modifiers, TestAppContext, VisualTestContext, WindowAppearance,
};
use lazy_client_core::{Delivery, Effect, Input, RequestError, SessionPhase, StartupState};
use lazy_domain::ConversationKind;
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, DisplayName, EventSeq, MessageDocument,
    MessageId, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    ApiError, BootstrapResponse, Conversation, ConversationUserState, ErrorCode, HistoryPage,
    Message, PROTOCOL_VERSION, SendMessageResponse, User, Workspace, WorkspaceBootstrap,
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

struct Fixture {
    model: Entity<AppModel>,
    effects: Rc<Recorder>,
    me: User,
    workspace: WorkspaceId,
    general: ConversationId,
}

/// A public channel as the server describes it.
fn channel(
    workspace: WorkspaceId,
    id: ConversationId,
    name: &str,
    topic: Option<&str>,
) -> Conversation {
    Conversation {
        id,
        workspace_id: workspace,
        kind: ConversationKind::Channel,
        name: Some(ChannelName::parse(name).unwrap()),
        topic: topic.map(str::to_owned),
        description: None,
        visibility: ChannelVisibility::Public,
        archived: false,
        is_default: name == "general",
        created_by: None,
        participants: vec![],
        member_count: 2,
    }
}

fn read_state(conversation: ConversationId) -> ConversationUserState {
    ConversationUserState {
        conversation_id: conversation,
        favorite: false,
        last_read: EventSeq::ZERO,
        unread_count: 0,
        mention_count: 0,
    }
}

fn user(name: &str) -> User {
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

fn start(cx: &mut TestAppContext) -> (Fixture, &mut VisualTestContext) {
    cx.update(|cx| lazy_ui::init(cx, WindowAppearance::Light));
    let effects = Rc::new(Recorder::default());
    let model = cx.new(|_| AppModel::new(effects.clone()));
    let (_root, cx) = cx.add_window_view({
        let model = model.clone();
        move |window, cx| RootView::new(model, window, cx)
    });
    let fixture = Fixture {
        model,
        effects,
        me: user("alice"),
        workspace: WorkspaceId::generate(),
        general: ConversationId::generate(),
    };
    (fixture, cx)
}

impl Fixture {
    fn dispatch(&self, cx: &mut VisualTestContext, input: Input) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.run_until_parked();
    }

    fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.effects.0.borrow_mut())
    }

    fn phase(&self, cx: &VisualTestContext) -> SessionPhase {
        self.model
            .read_with(cx, |model, _| model.core().phase().clone())
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
                threads: Vec::new(),
                groups: Vec::new(),
                workspace: Workspace {
                    id: self.workspace,
                    name: "Lazy".into(),
                    settings: Default::default(),
                },
                role: WorkspaceRole::Member,
                conversations: vec![channel(
                    self.workspace,
                    self.general,
                    "general",
                    Some("Company-wide chatter"),
                )],
                user_state: vec![read_state(self.general)],
                members: vec![self.me.clone(), user("bob")],
                head: EventSeq::ZERO,
            }],
        }
    }

    /// Takes the app from launch to a loaded `#general` with an empty first page of history.
    fn reach_general(&self, cx: &mut VisualTestContext) {
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
                    messages: vec![],
                    has_more: false,
                }),
            },
        );
    }

    fn message(&self, text: &str, seq: i64) -> Message {
        Message {
            attachments: vec![],
            thread_root: None,
            in_channel: false,
            thread: None,
            mentions: Default::default(),
            id: MessageId::generate(),
            conversation_id: self.general,
            author_id: self.me.id,
            document: MessageDocument::from_plain_text(text).unwrap(),
            created_at: Timestamp::now(),
            revision: 1,
            seq: EventSeq::new(seq).unwrap(),
            edited_at: None,
            reactions: vec![],
            reactions_as_of: EventSeq::ZERO,
            deleted: false,
        }
    }
}

#[gpui::test]
fn signing_in_with_the_keyboard_sends_a_login(cx: &mut TestAppContext) {
    let (app, cx) = start(cx);
    app.dispatch(cx, Input::Started(StartupState::default()));
    assert!(matches!(app.phase(cx), SessionPhase::SignedOut { .. }));

    // The username field has focus; Tab moves to the password, Enter submits.
    cx.simulate_input("Alice");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("correct horse");
    cx.simulate_keystrokes("enter");

    let login = app
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Login(request) => Some(request),
            _ => None,
        })
        .expect("Enter on the sign-in form emits a Login effect");
    assert_eq!(login.username.as_str(), "alice");
    assert_eq!(login.password.expose(), "correct horse");
    assert!(matches!(app.phase(cx), SessionPhase::SigningIn));
}

#[gpui::test]
fn a_rejected_sign_in_shows_the_reason_and_stays_on_the_form(cx: &mut TestAppContext) {
    let (app, cx) = start(cx);
    app.dispatch(cx, Input::Started(StartupState::default()));
    cx.simulate_input("al");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("pw");
    cx.simulate_keystrokes("enter");

    // Too-short input is refused by the core before anything is sent.
    assert!(
        app.take_effects()
            .iter()
            .all(|e| !matches!(e, Effect::Login(_)))
    );
    match app.phase(cx) {
        SessionPhase::SignedOut { last_error } => {
            assert!(last_error.is_some(), "an explanation is shown")
        }
        other => panic!("expected the sign-in form, got {other:?}"),
    }
}

#[gpui::test]
fn the_workspace_loads_and_a_message_goes_from_composer_to_confirmed(cx: &mut TestAppContext) {
    let (app, cx) = start(cx);
    app.reach_general(cx);
    assert!(matches!(app.phase(cx), SessionPhase::Ready));
    assert_eq!(
        app.model
            .read_with(cx, |m, _| m.core().selected_conversation()),
        Some(app.general)
    );

    // Typing reports the draft; Enter submits and journals the message before sending it.
    cx.simulate_input("hello 🎉");
    cx.simulate_keystrokes("shift-enter");
    cx.simulate_input("second line");
    assert_eq!(
        app.model
            .read_with(cx, |m, _| m.core().draft(app.general).to_owned()),
        "hello 🎉\nsecond line"
    );
    app.take_effects();
    cx.simulate_keystrokes("enter");
    let effects = app.take_effects();
    let operation = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry.operation_id),
            _ => None,
        })
        .expect("sending journals the message first");
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::SendMessage { .. })),
        "the request must wait for the journal write"
    );

    // The row appears as pending and the composer empties.
    let rows = app
        .model
        .read_with(cx, |m, _| m.core().timeline(app.general).to_vec());
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0].delivery, Delivery::Pending));
    assert_eq!(
        app.model
            .read_with(cx, |m, _| m.core().draft(app.general).to_owned()),
        ""
    );

    app.dispatch(cx, Input::SubmissionPersisted { operation });
    assert!(
        app.take_effects()
            .iter()
            .any(|e| matches!(e, Effect::SendMessage { .. }))
    );
    app.dispatch(
        cx,
        Input::SendResult {
            operation,
            result: Ok(SendMessageResponse {
                message: app.message("hello 🎉\nsecond line", 3),
            }),
        },
    );
    let rows = app
        .model
        .read_with(cx, |m, _| m.core().timeline(app.general).to_vec());
    assert_eq!(
        rows.len(),
        1,
        "confirmation updates the row; it does not add one"
    );
    assert!(rows[0].is_confirmed());
}

#[gpui::test]
fn every_delivery_state_renders(cx: &mut TestAppContext) {
    let (app, cx) = start(cx);
    app.reach_general(cx);

    // Pending, then an unknown outcome, then a definitive failure, then a confirmed message.
    cx.simulate_input("pending one");
    cx.simulate_keystrokes("enter");
    let operation = app
        .take_effects()
        .into_iter()
        .find_map(|e| match e {
            Effect::PersistSubmission(entry) => Some(entry.operation_id),
            _ => None,
        })
        .unwrap();
    app.dispatch(cx, Input::SubmissionPersisted { operation });
    app.dispatch(
        cx,
        Input::SendResult {
            operation,
            result: Err(lazy_client_core::RequestError::Unknown(
                "connection reset".into(),
            )),
        },
    );
    cx.run_until_parked();
    let state = app.model.read_with(cx, |m, _| {
        m.core().timeline(app.general)[0].delivery.clone()
    });
    assert!(matches!(state, Delivery::UnknownOutcome), "got {state:?}");

    // A connection loss and a notice also render.
    app.dispatch(
        cx,
        Input::Socket(lazy_client_core::SocketEvent::Closed {
            workspace: app.workspace,
            attempt: 1,
        }),
    );
    cx.run_until_parked();
}

#[gpui::test]
fn signing_out_returns_to_the_form(cx: &mut TestAppContext) {
    let (app, cx) = start(cx);
    app.reach_general(cx);
    app.take_effects();
    app.dispatch(cx, Input::SignOut);
    assert!(
        app.take_effects()
            .iter()
            .any(|e| matches!(e, Effect::SignOut))
    );
    assert!(matches!(app.phase(cx), SessionPhase::SignedOut { .. }));
}

impl Fixture {
    /// Sends `text` through the composer and returns its operation id after journaling.
    fn send_via_composer(
        &self,
        cx: &mut VisualTestContext,
        text: &str,
    ) -> lazy_domain::OperationId {
        cx.simulate_input(text);
        self.take_effects();
        cx.simulate_keystrokes("enter");
        self.take_effects()
            .into_iter()
            .find_map(|effect| match effect {
                Effect::PersistSubmission(entry) => Some(entry.operation_id),
                _ => None,
            })
            .expect("sending journals the message")
    }

    fn click(&self, cx: &mut VisualTestContext, selector: &'static str) {
        // A frame must have been drawn since the state last changed for bounds to exist.
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("nothing is labelled {selector:?} on screen"));
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();
    }
}

#[gpui::test]
fn clicking_send_again_resubmits_a_rejected_message_as_a_new_operation(cx: &mut TestAppContext) {
    let (app, cx) = start(cx);
    app.reach_general(cx);
    let first = app.send_via_composer(cx, "refused once");
    app.dispatch(cx, Input::SubmissionPersisted { operation: first });
    app.dispatch(
        cx,
        Input::SendResult {
            operation: first,
            result: Err(RequestError::Rejected(ApiError::new(
                ErrorCode::Forbidden,
                "no",
            ))),
        },
    );
    let row = app.model.read_with(cx, |m, _| {
        m.core().timeline(app.general)[0].delivery.clone()
    });
    assert!(matches!(row, Delivery::Failed { .. }), "got {row:?}");

    app.take_effects();
    app.click(cx, "retry-message");
    let resubmitted = app
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry),
            _ => None,
        })
        .expect("clicking Send again journals a new submission");
    assert_ne!(
        resubmitted.operation_id, first,
        "a definitive rejection retries as a new operation"
    );
    assert_eq!(resubmitted.document.plain_text(), "refused once");
}

#[gpui::test]
fn the_send_button_sends_what_is_in_the_composer(cx: &mut TestAppContext) {
    let (app, cx) = start(cx);
    app.reach_general(cx);
    cx.simulate_input("sent with the mouse");
    app.take_effects();
    app.click(cx, "send-message");
    let sent = app
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry.document.plain_text()),
            _ => None,
        })
        .expect("clicking Send submits the message");
    assert_eq!(sent, "sent with the mouse");
}

#[gpui::test]
fn the_copy_button_puts_the_message_text_on_the_clipboard(cx: &mut TestAppContext) {
    let (app, cx) = start(cx);
    app.reach_general(cx);
    let operation = app.send_via_composer(cx, "copy me\nsecond line");
    app.dispatch(cx, Input::SubmissionPersisted { operation });
    app.dispatch(
        cx,
        Input::SendResult {
            operation,
            result: Ok(SendMessageResponse {
                message: app.message("copy me\nsecond line", 5),
            }),
        },
    );
    // The button only appears while the pointer is over the row.
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let row = cx
        .debug_bounds("message-row-0")
        .expect("the message row is on screen");
    cx.simulate_mouse_move(row.center(), None, Modifiers::default());
    cx.run_until_parked();
    app.click(cx, "copy-message");
    let copied = cx.read_from_clipboard().and_then(|item| item.text());
    assert_eq!(copied.as_deref(), Some("copy me\nsecond line"));
}

#[gpui::test]
fn the_sign_out_button_signs_out(cx: &mut TestAppContext) {
    let (app, cx) = start(cx);
    app.reach_general(cx);
    app.take_effects();
    app.click(cx, "sign-out");
    assert!(
        app.take_effects()
            .iter()
            .any(|e| matches!(e, Effect::SignOut))
    );
    assert!(matches!(app.phase(cx), SessionPhase::SignedOut { .. }));
}

// ---- Virtualized timeline: bounded mounted rows, load-older on scroll, scroll anchoring ----

use gpui::{Bounds, Pixels, ScrollDelta, ScrollEvent, TouchPhase, point, px};

impl Fixture {
    fn messages(&self, range: std::ops::RangeInclusive<i64>) -> Vec<Message> {
        let authors = [self.me.clone(), user("bob"), user("zoe")];
        range
            .map(|seq| Message {
                attachments: vec![],
                author_id: authors[(seq as usize / 3) % authors.len()].id,
                ..self.message(&format!("message number {seq} with some body text"), seq)
            })
            .collect()
    }

    /// Like `reach_general`, but with `messages` as the newest page of history.
    fn reach_general_with(
        &self,
        cx: &mut VisualTestContext,
        messages: Vec<Message>,
        has_more: bool,
    ) {
        self.dispatch(
            cx,
            Input::Started(StartupState {
                session_restored: true,
                ..Default::default()
            }),
        );
        self.dispatch(cx, Input::BootstrapLoaded(Box::new(self.bootstrap())));
        let generation = self.fetch_generation();
        self.dispatch(
            cx,
            Input::HistoryLoaded {
                conversation: self.general,
                generation,
                result: Ok(HistoryPage { messages, has_more }),
            },
        );
    }

    /// The generation of the most recent history fetch the core asked for.
    fn fetch_generation(&self) -> u64 {
        self.effects
            .0
            .borrow()
            .iter()
            .rev()
            .find_map(|effect| match effect {
                Effect::FetchHistory { generation, .. } => Some(*generation),
                _ => None,
            })
            .expect("the core asked for history")
    }
}

/// Bounds of the row at `index`, if it is currently mounted. Selector names must be `'static`.
fn row_bounds(cx: &mut VisualTestContext, index: usize) -> Option<Bounds<Pixels>> {
    cx.debug_bounds(Box::leak(format!("message-row-{index}").into_boxed_str()))
}

fn redraw(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

fn mounted_rows(cx: &mut VisualTestContext, total: usize) -> Vec<usize> {
    redraw(cx);
    (0..total)
        .filter(|&index| row_bounds(cx, index).is_some())
        .collect()
}

fn scroll_timeline_up(cx: &mut VisualTestContext, pixels: f32) {
    cx.simulate_event(ScrollEvent {
        position: point(px(700.), px(300.)),
        delta: ScrollDelta::Pixels(point(px(0.), px(pixels))),
        touch_phase: TouchPhase::Moved,
        ..Default::default()
    });
    redraw(cx);
}

#[gpui::test]
fn only_a_bounded_window_of_a_ten_thousand_message_timeline_is_mounted(cx: &mut TestAppContext) {
    let (app, cx) = start(cx);
    app.reach_general_with(cx, app.messages(1..=10_000), true);
    let mounted = mounted_rows(cx, 10_000);
    assert!(!mounted.is_empty(), "the newest rows are on screen");
    assert!(
        mounted.len() < 150,
        "{} of 10,000 rows are mounted",
        mounted.len()
    );
    assert_eq!(
        mounted.last(),
        Some(&9_999),
        "the timeline starts at the newest message"
    );
}

#[gpui::test]
fn scrolling_to_the_top_asks_for_older_history_and_keeps_the_readers_place(
    cx: &mut TestAppContext,
) {
    let (app, cx) = start(cx);
    app.reach_general_with(cx, app.messages(101..=300), true);
    app.take_effects();

    // Flick upward repeatedly, as a person would, until the oldest loaded message is on screen.
    let mut mounted = mounted_rows(cx, 200);
    for _ in 0..30 {
        if mounted.first() == Some(&0) {
            break;
        }
        scroll_timeline_up(cx, 200_000.0);
        mounted = mounted_rows(cx, 200);
    }
    assert_eq!(
        mounted.first(),
        Some(&0),
        "scrolled to the oldest loaded message: {mounted:?}"
    );

    // Reaching the top requests the page before the oldest loaded message.
    let request = app
        .effects
        .0
        .borrow()
        .iter()
        .find_map(|effect| match effect {
            Effect::FetchHistory {
                before, generation, ..
            } => Some((*before, *generation)),
            _ => None,
        })
        .expect("scrolling to the top fetches older history");
    assert_eq!(request.0, Some(EventSeq::new(101).unwrap()));

    // Remember where one message sits, then prepend fifty older ones above it.
    let watched = mounted[1];
    let watched_id = app
        .model
        .read_with(cx, |m, _| m.core().timeline(app.general)[watched].key);
    let before = row_bounds(cx, watched)
        .expect("watched row is mounted")
        .origin
        .y;
    app.dispatch(
        cx,
        Input::HistoryLoaded {
            conversation: app.general,
            generation: request.1,
            result: Ok(HistoryPage {
                messages: app.messages(51..=100),
                has_more: false,
            }),
        },
    );
    // The restore waits two frames: one to measure the new rows, one to read the result.
    for _ in 0..4 {
        redraw(cx);
    }

    let moved_to = app.model.read_with(cx, |m, _| {
        m.core()
            .timeline(app.general)
            .iter()
            .position(|item| item.key == watched_id)
    });
    assert_eq!(
        moved_to,
        Some(watched + 50),
        "fifty rows were prepended above it"
    );
    let after = row_bounds(cx, watched + 50)
        .expect("the watched message is still on screen")
        .origin
        .y;
    assert!(
        (f32::from(after) - f32::from(before)).abs() <= 1.0,
        "the message jumped from y={before:?} to y={after:?} when older history loaded"
    );
}

// ---- Keyboard-only operation ----

#[gpui::test]
fn the_shell_can_be_driven_entirely_from_the_keyboard(cx: &mut TestAppContext) {
    let (app, cx) = start(cx);
    let random = ConversationId::generate();
    let mut bootstrap = app.bootstrap();
    bootstrap.workspaces[0]
        .conversations
        .push(channel(app.workspace, random, "random", None));
    bootstrap.workspaces[0].user_state.push(read_state(random));
    app.dispatch(
        cx,
        Input::Started(StartupState {
            session_restored: true,
            ..Default::default()
        }),
    );
    app.dispatch(cx, Input::BootstrapLoaded(Box::new(bootstrap)));
    let generation = app.fetch_generation();
    app.dispatch(
        cx,
        Input::HistoryLoaded {
            conversation: app.general,
            generation,
            result: Ok(HistoryPage {
                messages: vec![],
                has_more: false,
            }),
        },
    );

    // The composer has focus on arrival. Tab reaches the Attach button, then the emoji button, and
    // a third Tab reaches Send; Enter activates it.
    cx.simulate_input("typed with the keyboard");
    app.take_effects();
    cx.simulate_keystrokes("tab tab tab enter");
    let sent = app
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry.document.plain_text()),
            _ => None,
        })
        .expect("Tab then Enter on Send submits the message");
    assert_eq!(sent, "typed with the keyboard");

    // Shift+Tab walks back through everything between Send and the sidebar's channel list:
    // the emoji button, the Attach button, the composer, the timeline (one stop for all its messages), Details,
    // Favorite, the notification bell, Sign out, Preferences, "Start a conversation", the Direct
    // messages "+", and then the last channel row (random).
    cx.simulate_keystrokes(
        "shift-tab shift-tab shift-tab shift-tab shift-tab shift-tab shift-tab shift-tab shift-tab shift-tab \
         shift-tab shift-tab",
    );
    cx.simulate_keystrokes("enter");
    let selected = app
        .model
        .read_with(cx, |m, _| m.core().selected_conversation());
    assert_eq!(
        selected,
        Some(random),
        "Enter on a focused channel switches to it"
    );
    assert!(
        app.take_effects().iter().any(
            |e| matches!(e, Effect::FetchHistory { conversation, .. } if *conversation == random)
        ),
        "switching channels loads its history"
    );
}
