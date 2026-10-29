//! Message-level flows on GPUI's test platform: formatted messages, links, the hover toolbar and
//! the keyboard path to the same actions, editing in place, and deleting. The real views and the
//! real core run; a recording effect sink stands in for the runtime, and each test answers the
//! effects the way the runtime would.
//!
//! The test platform gives every character the same width and never reports a window as active,
//! so these prove behavior and wiring. Real glyph layout is checked in the offscreen renders
//! (`cargo run -p lazy-ui --example screens`), and nothing here exercises a real input method,
//! screen reader, or display.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, EntityInputHandler as _, Modifiers, TestAppContext, VisualTestContext,
    WindowAppearance,
};
use lazy_client_core::{
    ActionKind, ActionState, EditState, Effect, Input, JournalKind, StartupState,
};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    MessageDocument, MessageId, Permalink, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    BootstrapResponse, Conversation, ConversationUserState, Event, EventEnvelope, HistoryPage,
    Message, PROTOCOL_VERSION, ServerFrame, User, Workspace, WorkspaceBootstrap,
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
    bob: User,
    /// An app: the bot of an incoming webhook.
    deploy: User,
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
    let world = World {
        model,
        effects,
        root,
        me: person("alice"),
        bob: person("bob"),
        deploy: User {
            is_bot: true,
            deactivated: false,
            status: None,
            ..person("deploy")
        },
        workspace: WorkspaceId::generate(),
        general: ConversationId::generate(),
    };
    (world, cx)
}

impl World {
    fn dispatch(&self, cx: &mut VisualTestContext, input: Input) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.run_until_parked();
    }

    fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.effects.0.borrow_mut())
    }

    fn bootstrap(&self, role: WorkspaceRole) -> BootstrapResponse {
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
                    member_count: 2,
                }],
                user_state: vec![ConversationUserState {
                    conversation_id: self.general,
                    favorite: false,
                    last_read: EventSeq::ZERO,
                    unread_count: 0,
                    mention_count: 0,
                }],
                members: vec![self.me.clone(), self.bob.clone(), self.deploy.clone()],
                head: EventSeq::ZERO,
            }],
        }
    }

    /// A committed message whose content is composer source.
    fn message(&self, author: &User, source: &str, seq: i64) -> Message {
        Message {
            attachments: vec![],
            thread_root: None,
            in_channel: false,
            thread: None,
            mentions: Default::default(),
            id: MessageId::generate(),
            conversation_id: self.general,
            author_id: author.id,
            document: MessageDocument::from_markdown(source).unwrap(),
            created_at: Timestamp::now(),
            revision: 1,
            seq: EventSeq::new(seq).unwrap(),
            edited_at: None,
            reactions: vec![],
            reactions_as_of: EventSeq::ZERO,
            deleted: false,
        }
    }

    /// Signs in as a member and opens #general with `messages` as its history.
    fn reach(&self, cx: &mut VisualTestContext, messages: Vec<Message>) {
        self.reach_as(cx, WorkspaceRole::Member, messages);
    }

    fn reach_as(&self, cx: &mut VisualTestContext, role: WorkspaceRole, messages: Vec<Message>) {
        self.dispatch(
            cx,
            Input::Started(StartupState {
                session_restored: true,
                ..Default::default()
            }),
        );
        self.dispatch(cx, Input::BootstrapLoaded(Box::new(self.bootstrap(role))));
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

    fn timeline_len(&self, cx: &VisualTestContext) -> usize {
        self.model
            .read_with(cx, |m, _| m.core().timeline(self.general).len())
    }

    fn row(&self, cx: &mut VisualTestContext, index: usize) -> gpui::Bounds<gpui::Pixels> {
        draw(cx);
        cx.debug_bounds(Box::leak(format!("message-row-{index}").into_boxed_str()))
            .unwrap_or_else(|| panic!("row {index} is not on screen"))
    }

    fn hover_row(&self, cx: &mut VisualTestContext, index: usize) {
        let row = self.row(cx, index);
        cx.simulate_mouse_move(row.center(), None, Modifiers::default());
        cx.run_until_parked();
    }

    fn click(&self, cx: &mut VisualTestContext, selector: &'static str) {
        draw(cx);
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("nothing is labelled {selector:?} on screen"));
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();
    }

    /// Clicks near the start of the labelled element, where a short line of text begins.
    fn click_start(&self, cx: &mut VisualTestContext, selector: &'static str) {
        draw(cx);
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("nothing is labelled {selector:?} on screen"));
        cx.simulate_click(
            gpui::point(bounds.left() + gpui::px(6.), bounds.center().y),
            Modifiers::default(),
        );
        cx.run_until_parked();
    }

    fn on_screen(&self, cx: &mut VisualTestContext, selector: &'static str) -> bool {
        draw(cx);
        cx.debug_bounds(selector).is_some()
    }

    /// The index of the row the keyboard cursor is drawn on.
    fn cursor_row(&self, cx: &mut VisualTestContext) -> Option<usize> {
        draw(cx);
        let cursor = cx.debug_bounds("message-cursor")?;
        (0..self.timeline_len(cx)).find(|index| {
            cx.debug_bounds(Box::leak(format!("message-row-{index}").into_boxed_str()))
                .is_some_and(|row| row.contains(&cursor.center()))
        })
    }

    fn core_text(&self, cx: &VisualTestContext, id: MessageId) -> Option<String> {
        self.model.read_with(cx, |m, _| {
            m.core()
                .timeline(self.general)
                .iter()
                .find(|item| item.message_id() == Some(id))
                .map(|item| item.document.to_markdown())
        })
    }
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

fn clipboard(cx: &VisualTestContext) -> Option<String> {
    cx.read_from_clipboard().and_then(|item| item.text())
}

// ---- Formatted messages --------------------------------------------------------------------------

#[gpui::test]
fn every_kind_of_block_renders_and_the_code_block_copies_its_code(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let source = "*bold* _italic_ ~struck~ `code` [link](https://example.com)\n\
                  > quoted\n\
                  - one\n- two\n\
                  1. first\n2. second\n\
                  ```rust\nfn main() {\n    println!(\"hi\");\n}\n```";
    world.reach(cx, vec![world.message(&world.bob, source, 1)]);
    for selector in ["message-paragraph", "message-code-block"] {
        assert!(world.on_screen(cx, selector), "{selector} is drawn");
    }
    // Copy appears while the pointer is over the block.
    world.hover_row(cx, 0);
    let block = cx.debug_bounds("message-code-block").unwrap();
    cx.simulate_mouse_move(block.center(), None, Modifiers::default());
    world.click(cx, "copy-code");
    assert_eq!(
        clipboard(cx).as_deref(),
        Some("fn main() {\n    println!(\"hi\");\n}")
    );
}

#[gpui::test]
fn a_web_link_opens_in_the_system_and_the_message_stays_put(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(
        cx,
        vec![world.message(&world.bob, "[open the site](https://example.com/page)", 1)],
    );
    world.click_start(cx, "message-paragraph");
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com/page"),
        "the system opens the address"
    );
}

#[gpui::test]
fn a_permalink_to_a_loaded_message_jumps_to_it_and_highlights_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let target = world.message(&world.bob, "the message being pointed at", 1);
    let link = Permalink {
        workspace_id: world.workspace,
        conversation_id: world.general,
        message_id: target.id,
    };
    let pointer = world.message(&world.bob, &format!("[over there]({link})"), 2);
    world.reach(cx, vec![target, pointer]);
    assert!(!world.on_screen(cx, "highlighted-row"));

    world.click_start(cx, "message-paragraph");
    assert!(
        world.on_screen(cx, "highlighted-row"),
        "the target is highlighted after the jump"
    );
    let jump = world.model.read_with(cx, |m, _| m.core().jump_target());
    assert!(
        jump.is_none(),
        "the view tells the core it has handled the jump"
    );
    // Clicking a message does not take the keyboard from the composer.
    cx.simulate_input("typed after the click");
    let draft = world
        .model
        .read_with(cx, |m, _| m.core().draft(world.general).to_owned());
    assert_eq!(draft, "typed after the click");

    // The highlight fades by itself.
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(5));
    cx.run_until_parked();
    assert!(!world.on_screen(cx, "highlighted-row"));
}

// ---- The toolbar and the keyboard menu ------------------------------------------------------------

#[gpui::test]
fn other_peoples_messages_offer_only_copy_actions(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![world.message(&world.bob, "from bob", 1)]);
    world.hover_row(cx, 0);
    assert!(world.on_screen(cx, "copy-link"));
    assert!(world.on_screen(cx, "copy-message"));
    assert!(
        !world.on_screen(cx, "edit-message"),
        "only the author edits"
    );
    assert!(
        !world.on_screen(cx, "delete-message"),
        "a member cannot delete someone else's message"
    );
}

#[gpui::test]
fn an_admin_may_delete_other_peoples_messages_but_not_edit_them(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(
        cx,
        WorkspaceRole::Admin,
        vec![world.message(&world.bob, "from bob", 1)],
    );
    world.hover_row(cx, 0);
    assert!(world.on_screen(cx, "delete-message"));
    assert!(!world.on_screen(cx, "edit-message"));
}

#[gpui::test]
fn an_apps_messages_wear_an_app_badge_and_people_do_not(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![world.message(&world.bob, "from bob", 1)]);
    assert!(
        !world.on_screen(cx, "app-badge"),
        "a person's message has no badge"
    );
    world.event(
        cx,
        2,
        Event::MessageCreated {
            message: world.message(&world.deploy, "Deploy finished", 2),
            operation_id: None,
        },
    );
    draw(cx);
    assert!(
        world.on_screen(cx, "app-badge"),
        "a webhook's bot is marked as an app"
    );
}

#[gpui::test]
fn nobody_edits_an_apps_message_and_only_a_moderator_deletes_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(
        cx,
        WorkspaceRole::Admin,
        vec![world.message(&world.deploy, "Deploy finished", 1)],
    );
    world.hover_row(cx, 0);
    assert!(world.on_screen(cx, "copy-message"), "it can be copied");
    assert!(
        !world.on_screen(cx, "edit-message"),
        "nobody is its author, so nobody edits it"
    );
    assert!(
        world.on_screen(cx, "delete-message"),
        "an admin can remove it"
    );

    let (world, cx) = start(cx);
    world.reach(cx, vec![world.message(&world.deploy, "Deploy finished", 1)]);
    world.hover_row(cx, 0);
    assert!(!world.on_screen(cx, "edit-message"));
    assert!(
        !world.on_screen(cx, "delete-message"),
        "a member cannot delete what an app posted"
    );
}

#[gpui::test]
fn the_author_may_edit_and_delete_their_own_messages(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![world.message(&world.me, "mine", 1)]);
    world.hover_row(cx, 0);
    assert!(world.on_screen(cx, "edit-message"));
    assert!(world.on_screen(cx, "delete-message"));
}

#[gpui::test]
fn copy_link_puts_the_permalink_on_the_clipboard(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let message = world.message(&world.bob, "link to me", 1);
    let expected = Permalink {
        workspace_id: world.workspace,
        conversation_id: world.general,
        message_id: message.id,
    }
    .to_string();
    world.reach(cx, vec![message]);
    world.hover_row(cx, 0);
    world.click(cx, "copy-link");
    assert_eq!(clipboard(cx), Some(expected));
}

#[gpui::test]
fn arrow_keys_move_between_messages_without_a_tab_stop_per_row(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let messages = (1..=4)
        .map(|n| world.message(&world.bob, &format!("message {n}"), n))
        .collect();
    world.reach(cx, messages);
    assert_eq!(
        world.cursor_row(cx),
        None,
        "nothing is marked until the timeline has focus"
    );

    // The composer has focus on arrival; one Shift+Tab reaches the timeline.
    cx.simulate_keystrokes("shift-tab");
    assert_eq!(
        world.cursor_row(cx),
        Some(3),
        "the cursor starts on the newest message"
    );
    cx.simulate_keystrokes("up");
    assert_eq!(world.cursor_row(cx), Some(2));
    cx.simulate_keystrokes("up up");
    assert_eq!(world.cursor_row(cx), Some(0));
    cx.simulate_keystrokes("up");
    assert_eq!(world.cursor_row(cx), Some(0), "it stops at the oldest");
    cx.simulate_keystrokes("down");
    assert_eq!(world.cursor_row(cx), Some(1));
    cx.simulate_keystrokes("end");
    assert_eq!(world.cursor_row(cx), Some(3));
    cx.simulate_keystrokes("home");
    assert_eq!(world.cursor_row(cx), Some(0));

    // Leaving the timeline hides the cursor; the four rows were never separate tab stops.
    cx.simulate_keystrokes("tab");
    assert_eq!(world.cursor_row(cx), None, "one Tab leaves the timeline");
}

#[gpui::test]
fn enter_opens_the_actions_menu_and_the_keyboard_can_pick_from_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let message = world.message(&world.bob, "act on me", 1);
    let expected = Permalink {
        workspace_id: world.workspace,
        conversation_id: world.general,
        message_id: message.id,
    }
    .to_string();
    world.reach(cx, vec![message]);

    cx.simulate_keystrokes("shift-tab");
    assert!(!world.on_screen(cx, "message-actions"));
    cx.simulate_keystrokes("enter");
    assert!(
        world.on_screen(cx, "message-actions"),
        "Enter opens the menu"
    );
    assert!(world.on_screen(cx, "message-actions-copy-link"));
    assert!(world.on_screen(cx, "message-actions-copy-text"));
    assert!(!world.on_screen(cx, "message-actions-edit"));

    // Escape closes it and focus returns to the timeline.
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "message-actions"));
    assert_eq!(
        world.cursor_row(cx),
        Some(0),
        "the cursor is still on the message"
    );

    // Someone else's message offers Add reaction…, Reply in thread, Copy link, Copy text.
    cx.simulate_keystrokes("enter down down enter");
    assert!(
        !world.on_screen(cx, "message-actions"),
        "picking closes the menu"
    );
    assert_eq!(clipboard(cx), Some(expected), "the third item is Copy link");

    cx.simulate_keystrokes("enter down down down enter");
    assert_eq!(
        clipboard(cx).as_deref(),
        Some("act on me"),
        "the fourth item is Copy text"
    );
}

#[gpui::test]
fn the_menu_for_a_persons_own_message_lists_edit_and_delete(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![world.message(&world.me, "mine", 1)]);
    cx.simulate_keystrokes("shift-tab enter");
    for item in ["react", "reply", "edit", "delete", "copy-link", "copy-text"] {
        let selector: &'static str = Box::leak(format!("message-actions-{item}").into_boxed_str());
        assert!(world.on_screen(cx, selector), "{item} is offered");
    }
}

// ---- Composer ------------------------------------------------------------------------------------

#[gpui::test]
fn the_formatting_shortcuts_wrap_the_selection_in_the_composer(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    cx.simulate_input("make it loud");
    cx.simulate_keystrokes("shift-ctrl-left");
    cx.simulate_keystrokes("ctrl-b");
    let draft = world
        .model
        .read_with(cx, |m, _| m.core().draft(world.general).to_owned());
    assert_eq!(
        draft, "make it *loud*",
        "the draft is what a send would parse"
    );
}

// ---- Deleting ------------------------------------------------------------------------------------

fn open_delete_dialog(world: &World, cx: &mut VisualTestContext, message: MessageId) {
    let general = world.general;
    world.root.update_in(cx, |root, window, cx| {
        let shell = root.shell().expect("the shell is showing").clone();
        shell.update(cx, |shell, cx| {
            shell.open_dialog(
                ShellDialog::DeleteMessage {
                    conversation: general,
                    message,
                },
                window,
                cx,
            )
        });
    });
    cx.run_until_parked();
}

#[gpui::test]
fn deleting_asks_first_and_cancel_keeps_the_message(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let message = world.message(&world.me, "keep me", 1);
    let id = message.id;
    world.reach(cx, vec![message]);
    open_delete_dialog(&world, cx, id);
    assert!(world.on_screen(cx, "delete-message-confirm"));
    assert!(
        world.on_screen(cx, "delete-preview"),
        "the dialog shows what is about to go"
    );

    // Focus starts on Cancel, so a reflexive Enter is the safe answer.
    cx.simulate_keystrokes("enter");
    assert!(
        !world.on_screen(cx, "delete-message-confirm"),
        "Enter on Cancel closes the dialog"
    );
    assert!(
        world
            .take_effects()
            .iter()
            .all(|e| !matches!(e, Effect::DeleteMessage { .. })),
        "nothing was sent"
    );
    assert_eq!(world.timeline_len(cx), 1);

    open_delete_dialog(&world, cx, id);
    cx.simulate_keystrokes("escape");
    assert!(
        !world.on_screen(cx, "delete-message-confirm"),
        "Escape closes it too"
    );
}

#[gpui::test]
fn confirming_sends_the_deletion_and_closes_when_it_goes_through(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let message = world.message(&world.me, "delete me", 1);
    let id = message.id;
    world.reach(cx, vec![message.clone()]);
    world.take_effects();
    open_delete_dialog(&world, cx, id);
    world.click(cx, "delete-message-confirm");

    let state = world.model.read_with(cx, |m, _| {
        m.core().action(ActionKind::DeleteMessage(id)).clone()
    });
    assert_eq!(state, ActionState::InFlight);
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::DeleteMessage { message } if *message == id)),
        "the core asks the runtime to delete it"
    );
    assert!(
        world.on_screen(cx, "delete-message-confirm"),
        "the dialog waits for the result"
    );

    let mut tombstone = message;
    tombstone.deleted = true;
    tombstone.revision = 2;
    world.dispatch(
        cx,
        Input::DeleteResult {
            message: id,
            result: Ok(lazy_protocol::MessageResponse { message: tombstone }),
        },
    );
    assert!(
        !world.on_screen(cx, "delete-message-confirm"),
        "success closes the dialog"
    );
    assert_eq!(world.timeline_len(cx), 0, "the message leaves the timeline");
}

#[gpui::test]
fn a_failed_deletion_is_explained_in_the_dialog_and_can_be_retried(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let message = world.message(&world.me, "stubborn", 1);
    let id = message.id;
    world.reach(cx, vec![message]);
    open_delete_dialog(&world, cx, id);
    world.click(cx, "delete-message-confirm");
    world.dispatch(
        cx,
        Input::DeleteResult {
            message: id,
            result: Err(lazy_client_core::RequestError::Rejected(
                lazy_protocol::ApiError::new(
                    lazy_protocol::ErrorCode::Forbidden,
                    "You can't delete that.",
                ),
            )),
        },
    );
    assert!(world.on_screen(cx, "delete-error"), "the reason is shown");
    assert_eq!(world.timeline_len(cx), 1, "the message is still there");
    world.take_effects();
    world.click(cx, "delete-message-confirm");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::DeleteMessage { .. })),
        "the same button tries again"
    );
}

// ---- Editing in place ----------------------------------------------------------------------------

/// Opens the editor on the person's only message through the toolbar.
fn begin_edit(world: &World, cx: &mut VisualTestContext) {
    world.hover_row(cx, 0);
    world.click(cx, "edit-message");
}

fn editor_text(cx: &mut VisualTestContext) -> String {
    // The editor's input has focus; select everything and read it back through the clipboard.
    cx.simulate_keystrokes("ctrl-a ctrl-c");
    clipboard(cx).unwrap_or_default()
}

#[gpui::test]
fn the_editor_opens_on_the_messages_source_and_escape_cancels(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let message = world.message(&world.me, "some *bold* text", 1);
    world.reach(cx, vec![message]);
    assert!(!world.on_screen(cx, "message-editor"));
    begin_edit(&world, cx);
    assert!(world.on_screen(cx, "message-editor"));
    assert_eq!(
        editor_text(cx),
        "some *bold* text",
        "the editor shows the source"
    );

    cx.simulate_keystrokes("escape");
    assert!(
        !world.on_screen(cx, "message-editor"),
        "Escape closes the editor"
    );
    assert!(
        world
            .take_effects()
            .iter()
            .all(|e| !matches!(e, Effect::PersistSubmission(_))),
        "cancelling sends nothing"
    );
}

#[gpui::test]
fn saving_journals_the_edit_then_sends_it_and_closes_on_success(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let message = world.message(&world.me, "first draft", 1);
    let id = message.id;
    world.reach(cx, vec![message.clone()]);
    world.take_effects();
    begin_edit(&world, cx);
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("second *draft*");
    cx.simulate_keystrokes("enter");

    // The edit is journaled before it is sent.
    let entry = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry),
            _ => None,
        })
        .expect("an edit is journaled first");
    assert!(matches!(entry.kind, JournalKind::Edit { message_id, .. } if message_id == id));
    assert_eq!(entry.document.to_markdown(), "second *draft*");
    assert!(
        world.on_screen(cx, "message-editor"),
        "the editor stays while saving"
    );
    world.dispatch(
        cx,
        Input::SubmissionPersisted {
            operation: entry.operation_id,
        },
    );

    let request = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::EditMessage { message, request } if message == id => Some(request),
            _ => None,
        })
        .expect("then it is sent");
    assert_eq!(request.base_revision, 1);

    let mut edited = message;
    edited.document = request.document.clone();
    edited.revision = 2;
    edited.edited_at = Some(Timestamp::now());
    world.dispatch(
        cx,
        Input::EditResult {
            operation: request.operation_id,
            result: Ok(lazy_protocol::MessageResponse { message: edited }),
        },
    );
    assert!(
        !world.on_screen(cx, "message-editor"),
        "success closes the editor"
    );
    assert!(
        world.on_screen(cx, "edited-marker"),
        "the message now says it was edited"
    );
    assert_eq!(world.core_text(cx, id).as_deref(), Some("second *draft*"));
}

#[gpui::test]
fn emptying_the_text_offers_deleting_instead_of_saving(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![world.message(&world.me, "going away", 1)]);
    world.take_effects();
    begin_edit(&world, cx);
    cx.simulate_keystrokes("ctrl-a backspace enter");
    assert!(
        world.on_screen(cx, "edit-delete"),
        "Delete message is offered"
    );
    assert!(world.on_screen(cx, "edit-keep"));
    assert!(
        world
            .take_effects()
            .iter()
            .all(|e| !matches!(e, Effect::PersistSubmission(_))),
        "an empty message is not saved"
    );
    world.click(cx, "edit-keep");
    assert!(!world.on_screen(cx, "edit-delete"));
    assert!(world.on_screen(cx, "message-editor"));

    cx.simulate_keystrokes("ctrl-a backspace enter");
    world.click(cx, "edit-delete");
    assert!(
        world.on_screen(cx, "delete-message-confirm"),
        "choosing delete goes through the confirmation"
    );
}

#[gpui::test]
fn a_conflict_keeps_the_persons_text_and_names_the_other_change(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let message = world.message(&world.me, "original", 1);
    let id = message.id;
    world.reach(cx, vec![message.clone()]);
    world.take_effects();
    begin_edit(&world, cx);
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("my version");
    cx.simulate_keystrokes("enter");
    let entry = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry),
            _ => None,
        })
        .unwrap();
    world.dispatch(
        cx,
        Input::SubmissionPersisted {
            operation: entry.operation_id,
        },
    );
    let request = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::EditMessage { request, .. } => Some(request),
            _ => None,
        })
        .unwrap();

    // The server rejects it as stale, and the newer version arrives.
    world.dispatch(
        cx,
        Input::EditResult {
            operation: request.operation_id,
            result: Err(lazy_client_core::RequestError::Rejected(
                lazy_protocol::ApiError::new(
                    lazy_protocol::ErrorCode::StaleRevision,
                    "The message changed.",
                ),
            )),
        },
    );
    let mut newer = message;
    newer.document = MessageDocument::from_markdown("their version").unwrap();
    newer.revision = 2;
    newer.edited_at = Some(Timestamp::now());
    world.dispatch(
        cx,
        Input::MessageFetched {
            message: id,
            result: Ok(lazy_protocol::MessageResponse { message: newer }),
        },
    );

    let state = world
        .model
        .read_with(cx, |m, _| m.core().timeline(world.general)[0].edit.clone());
    assert!(matches!(state, EditState::Conflict { .. }), "got {state:?}");
    assert!(
        world.on_screen(cx, "edit-conflict"),
        "the editor says someone else changed it"
    );
    assert_eq!(
        editor_text(cx),
        "my version",
        "the person's text is still editable"
    );
}

#[gpui::test]
fn up_in_the_empty_composer_edits_the_latest_message_the_person_wrote(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(
        cx,
        vec![
            world.message(&world.me, "my older message", 1),
            world.message(&world.me, "my latest *message*", 2),
            world.message(&world.bob, "bob has the last word", 3),
        ],
    );
    cx.simulate_keystrokes("up");
    assert!(world.on_screen(cx, "message-editor"), "Up opens the editor");
    assert_eq!(
        editor_text(cx),
        "my latest *message*",
        "it is the person's newest message, not the newest message"
    );
}

#[gpui::test]
fn up_does_nothing_when_the_person_has_written_nothing_to_edit(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(
        cx,
        vec![world.message(&world.bob, "only bob has written", 1)],
    );
    cx.simulate_keystrokes("up");
    assert!(
        !world.on_screen(cx, "message-editor"),
        "nothing of theirs to edit"
    );
}

#[gpui::test]
fn up_with_a_draft_in_the_composer_moves_the_caret_instead(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![world.message(&world.me, "mine", 1)]);
    cx.simulate_input("a draft");
    cx.simulate_keystrokes("up");
    assert!(!world.on_screen(cx, "message-editor"));
}

#[gpui::test]
fn editing_the_failed_text_again_reopens_the_editor_with_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let message = world.message(&world.me, "original", 1);
    world.reach(cx, vec![message]);
    world.take_effects();
    begin_edit(&world, cx);
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("unsaved words");
    cx.simulate_keystrokes("enter");
    let entry = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry),
            _ => None,
        })
        .unwrap();
    // The journal write fails, so the edit is not saved and the editor says so.
    world.dispatch(
        cx,
        Input::SubmissionPersistFailed {
            operation: entry.operation_id,
            message: "disk full".into(),
        },
    );
    assert!(
        world.on_screen(cx, "message-editor"),
        "the editor is still open"
    );
    assert!(
        world.on_screen(cx, "edit-failed"),
        "it says why nothing was saved"
    );
    assert_eq!(editor_text(cx), "unsaved words", "and keeps the text");
}

// ---- Permalinks to channels the person is not in ---------------------------------------------------

#[gpui::test]
fn a_permalink_to_an_unjoined_channel_offers_to_join_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    world.take_effects();
    let elsewhere = ConversationId::generate();
    let message = Message {
        attachments: vec![],
        conversation_id: elsewhere,
        ..world.message(&world.bob, "said somewhere else", 5)
    };
    world.dispatch(
        cx,
        Input::OpenPermalink(Permalink {
            workspace_id: world.workspace,
            conversation_id: elsewhere,
            message_id: message.id,
        }),
    );
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::FetchMessage { message: m } if *m == message.id)),
        "the core asks for the message"
    );
    world.dispatch(
        cx,
        Input::MessageFetched {
            message: message.id,
            result: Ok(lazy_protocol::MessageResponse { message }),
        },
    );
    assert!(world.on_screen(cx, "notice-banner"), "the notice explains");
    assert!(
        world.on_screen(cx, "join-offer"),
        "and joining is one click away"
    );

    world.click(cx, "join-offer");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::JoinChannel { .. })),
        "clicking joins the channel"
    );
}

// ---- Changes arriving from other people and devices ----------------------------------------------

impl World {
    /// Delivers a realtime event for #general as the server would.
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
}

#[gpui::test]
fn someone_else_editing_a_message_shows_the_edited_marker(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let message = world.message(&world.bob, "typo", 1);
    world.reach(cx, vec![message.clone()]);
    assert!(!world.on_screen(cx, "edited-marker"));
    let mut edited = message;
    edited.document = MessageDocument::from_markdown("fixed *now*").unwrap();
    edited.revision = 2;
    edited.edited_at = Some(Timestamp::now());
    world.event(
        cx,
        2,
        Event::MessageEdited {
            previous_mentions: Default::default(),
            message: edited,
            operation_id: None,
        },
    );
    assert!(world.on_screen(cx, "edited-marker"));
}

#[gpui::test]
fn deleting_the_message_being_edited_closes_the_editor(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let message = world.message(&world.me, "about to vanish", 1);
    world.reach(cx, vec![message.clone()]);
    begin_edit(&world, cx);
    assert!(world.on_screen(cx, "message-editor"));
    let mut tombstone = message;
    tombstone.deleted = true;
    tombstone.revision = 2;
    world.event(
        cx,
        2,
        Event::MessageDeleted {
            message: tombstone,
            had_mentions: Default::default(),
        },
    );
    assert!(
        !world.on_screen(cx, "message-editor"),
        "the editor goes with the message"
    );
    assert_eq!(world.timeline_len(cx), 0);
}

#[gpui::test]
fn the_keyboard_cursor_survives_the_message_under_it_being_deleted(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let first = world.message(&world.bob, "one", 1);
    let second = world.message(&world.bob, "two", 2);
    world.reach(cx, vec![first, second.clone()]);
    cx.simulate_keystrokes("shift-tab");
    assert_eq!(world.cursor_row(cx), Some(1));
    let mut tombstone = second;
    tombstone.deleted = true;
    tombstone.revision = 2;
    world.event(
        cx,
        3,
        Event::MessageDeleted {
            message: tombstone,
            had_mentions: Default::default(),
        },
    );
    // The row is gone; the next arrow press lands on a message that still exists.
    cx.simulate_keystrokes("up");
    assert_eq!(world.cursor_row(cx), Some(0));
}

#[gpui::test]
fn escape_during_an_input_method_composition_does_not_cancel_the_edit(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![world.message(&world.me, "mine", 1)]);
    begin_edit(&world, cx);
    let input = world
        .root
        .read_with(cx, |root, cx| {
            root.shell()
                .expect("the shell is showing")
                .read(cx)
                .timeline()
                .read(cx)
                .editor_input()
        })
        .expect("the editor is open");

    // A composition in progress, as an input method reports it (simulated, not a real IME).
    input.update_in(cx, |input, window, cx| {
        input.replace_and_mark_text_in_range(None, "かな", None, window, cx)
    });
    assert!(input.read_with(cx, |input, _| input.is_composing()));
    cx.simulate_keystrokes("escape");
    assert!(
        world.on_screen(cx, "message-editor"),
        "Escape belongs to the composition, so the editor stays"
    );

    // Once the composition has ended, Escape cancels as usual.
    input.update_in(cx, |input, window, cx| {
        input.unmark_text(window, cx);
    });
    assert!(!input.read_with(cx, |input, _| input.is_composing()));
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "message-editor"));
}

// ---- Where the keyboard goes when an edit ends ---------------------------------------------------

/// The server accepts the edit that was just saved, as the runtime would report it.
fn server_accepts_the_edit(world: &World, cx: &mut VisualTestContext, original: Message) {
    let entry = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry),
            _ => None,
        })
        .expect("an edit is journaled first");
    world.dispatch(
        cx,
        Input::SubmissionPersisted {
            operation: entry.operation_id,
        },
    );
    let request = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::EditMessage { request, .. } => Some(request),
            _ => None,
        })
        .expect("then it is sent");
    let mut edited = original;
    edited.document = request.document.clone();
    edited.revision = 2;
    edited.edited_at = Some(Timestamp::now());
    world.dispatch(
        cx,
        Input::EditResult {
            operation: request.operation_id,
            result: Ok(lazy_protocol::MessageResponse { message: edited }),
        },
    );
    assert!(
        !world.on_screen(cx, "message-editor"),
        "success closes the editor"
    );
}

/// Whether typing now goes to the composer: the draft grows by what is typed.
fn typing_reaches_the_composer(world: &World, cx: &mut VisualTestContext) -> bool {
    let before = world
        .model
        .read_with(cx, |m, _| m.core().draft(world.general).to_owned());
    cx.simulate_input("x");
    let after = world
        .model
        .read_with(cx, |m, _| m.core().draft(world.general).to_owned());
    after == format!("{before}x")
}

#[gpui::test]
fn an_edit_begun_with_up_returns_to_the_composer_after_saving(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let message = world.message(&world.me, "mine", 1);
    world.reach(cx, vec![message.clone()]);
    world.take_effects();

    cx.simulate_keystrokes("up");
    assert!(world.on_screen(cx, "message-editor"));
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("changed");
    cx.simulate_keystrokes("enter");
    server_accepts_the_edit(&world, cx, message);

    assert!(
        typing_reaches_the_composer(&world, cx),
        "typed characters go to the composer"
    );
    assert_eq!(
        world.cursor_row(cx),
        None,
        "the timeline has no cursor showing"
    );

    // Shift+Tab from the composer reaches the timeline, and from there the actions menu: the
    // person's own message offers Add reaction…, Reply in thread, Edit, Delete, Copy link, Copy
    // text, in that order (and a toggle for each reaction it already has, after Add reaction…).
    cx.simulate_keystrokes("shift-tab");
    assert_eq!(
        world.cursor_row(cx),
        Some(0),
        "the timeline has the keyboard now"
    );
    cx.simulate_keystrokes("enter down down down enter");
    assert!(
        world.on_screen(cx, "delete-message-confirm"),
        "the fourth item is Delete, and it asks first"
    );
    // The dialog starts on Cancel, so Enter backs out.
    cx.simulate_keystrokes("enter");
    assert!(!world.on_screen(cx, "delete-message-confirm"));
}

#[gpui::test]
fn escape_on_an_edit_begun_with_up_returns_to_the_composer(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![world.message(&world.me, "mine", 1)]);
    cx.simulate_keystrokes("up");
    assert!(world.on_screen(cx, "message-editor"));
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "message-editor"));
    assert!(typing_reaches_the_composer(&world, cx));
    cx.simulate_keystrokes("shift-tab");
    assert_eq!(world.cursor_row(cx), Some(0));
}

#[gpui::test]
fn an_edit_begun_from_the_toolbar_returns_to_the_message(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let message = world.message(&world.me, "mine", 1);
    world.reach(cx, vec![message.clone()]);
    world.take_effects();

    // Escape.
    begin_edit(&world, cx);
    cx.simulate_keystrokes("escape");
    assert_eq!(
        world.cursor_row(cx),
        Some(0),
        "the cursor is on the message"
    );
    assert!(
        !typing_reaches_the_composer(&world, cx),
        "typing does not go to the composer"
    );

    // Save.
    begin_edit(&world, cx);
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("changed");
    cx.simulate_keystrokes("enter");
    server_accepts_the_edit(&world, cx, message);
    assert_eq!(
        world.cursor_row(cx),
        Some(0),
        "the row has the keyboard again"
    );
    assert!(!typing_reaches_the_composer(&world, cx));
}

#[gpui::test]
fn an_edit_begun_from_the_actions_menu_returns_to_the_message(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let message = world.message(&world.me, "mine", 1);
    world.reach(cx, vec![message.clone()]);
    world.take_effects();

    // Add reaction…, Reply in thread, then Edit.
    cx.simulate_keystrokes("shift-tab enter down down enter");
    assert!(
        world.on_screen(cx, "message-editor"),
        "the third item is Edit"
    );
    cx.simulate_keystrokes("escape");
    assert_eq!(world.cursor_row(cx), Some(0));
    assert!(!typing_reaches_the_composer(&world, cx));

    cx.simulate_keystrokes("enter down down enter ctrl-a");
    cx.simulate_input("changed");
    cx.simulate_keystrokes("enter");
    server_accepts_the_edit(&world, cx, message);
    assert_eq!(world.cursor_row(cx), Some(0));
    assert!(!typing_reaches_the_composer(&world, cx));
}

#[gpui::test]
fn if_the_message_is_gone_the_keyboard_goes_to_the_composer(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let message = world.message(&world.me, "about to vanish", 1);
    world.reach(cx, vec![message.clone()]);
    // Began from the row, with the row then deleted by someone else.
    begin_edit(&world, cx);
    let mut tombstone = message;
    tombstone.deleted = true;
    tombstone.revision = 2;
    world.event(
        cx,
        2,
        Event::MessageDeleted {
            message: tombstone,
            had_mentions: Default::default(),
        },
    );
    assert!(!world.on_screen(cx, "message-editor"));
    assert!(
        typing_reaches_the_composer(&world, cx),
        "with the row gone, the composer is where the keyboard belongs"
    );
}
