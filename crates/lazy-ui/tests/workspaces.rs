//! Slice 3G on GPUI's test platform: the workspace rail, "Add a workspace", the profile and
//! appearance sections of Preferences, and people's pictures. The real views and the real core
//! run; a recording effect sink stands in for the runtime.
//!
//! Every keyboard claim is made with real key events through GPUI's dispatch, every mouse claim
//! with real mouse events. The test platform gives every character the same width and never
//! reports a window as active; whether a screen looks right is decided by the offscreen renders
//! (`.dev/screens-3g`). Nothing here exercises a real input method, a real file dialog, or a
//! screen reader.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, Modifiers, TestAppContext, VisualTestContext, WindowAppearance,
};
use lazy_client_core::{
    ActionKind, ActionOutcome, ActionState, Appearance, Effect, FileSource, ImageData, Input,
    MotionChoice, RequestError, StartupState, ThemeChoice,
};
use lazy_domain::notification::NotificationSettings;
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    MessageDocument, MessageId, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    ApiError, BootstrapResponse, Conversation, ConversationUserState, ErrorCode, HistoryPage,
    Message, PROTOCOL_VERSION, User, Workspace, WorkspaceBootstrap,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::dialogs::Way;
use lazy_ui::image_store::{ImageKey, ImageStore, Slot};
use lazy_ui::rail::{COMPACT_RAIL_WIDTH, RAIL_WIDTH, RailView};
use lazy_ui::root_view::RootView;
use lazy_ui::theme::{Theme, ThemeMode};

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
    carol: User,
    /// The workspaces on the rail, in order: the first is the one that opens.
    ids: [WorkspaceId; 3],
    general: ConversationId,
    /// Which of the people have a picture (version 1).
    bob_has_picture: bool,
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
    let world = World {
        model,
        effects,
        root,
        me: person("myself", "Me Myself"),
        bob: person("bob", "Bob Okafor"),
        carol: person("carol", "Carol Mbeki"),
        ids: [
            WorkspaceId::generate(),
            WorkspaceId::generate(),
            WorkspaceId::generate(),
        ],
        general: ConversationId::generate(),
        bob_has_picture: false,
    };
    (world, cx)
}

/// Tall enough that Preferences shows every section without scrolling.
const HEIGHT: f32 = 2600.;

/// Two frames: what is laid out in one is what the mouse can hit in the next.
fn draw(cx: &mut VisualTestContext) {
    for _ in 0..2 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

/// A tiny valid picture.
fn png() -> Vec<u8> {
    let mut picture = image::RgbaImage::new(8, 8);
    for (x, y, pixel) in picture.enumerate_pixels_mut() {
        *pixel = image::Rgba([(x * 30) as u8, (y * 30) as u8, 120, 255]);
    }
    let mut out = std::io::Cursor::new(Vec::new());
    picture
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("encode");
    out.into_inner()
}

impl World {
    fn channel(&self, workspace: WorkspaceId, id: ConversationId, name: &str) -> Conversation {
        Conversation {
            id,
            workspace_id: workspace,
            kind: ConversationKind::Channel,
            name: Some(ChannelName::parse(name).unwrap()),
            topic: None,
            description: None,
            visibility: ChannelVisibility::Public,
            archived: false,
            is_default: name == "general",
            created_by: None,
            participants: vec![],
            member_count: 3,
        }
    }

    /// A workspace the person belongs to, with a #general holding `unread` unread messages of
    /// which `mentions` mention them.
    fn workspace(
        &self,
        id: WorkspaceId,
        name: &str,
        general: ConversationId,
        unread: u32,
        mentions: u32,
    ) -> WorkspaceBootstrap {
        let bob = User {
            avatar_version: u32::from(self.bob_has_picture),
            ..self.bob.clone()
        };
        WorkspaceBootstrap {
            saved: Vec::new(),
            notification_settings: NotificationSettings::default(),
            conversation_notification_prefs: Vec::new(),
            custom_emoji: Vec::new(),
            threads: Vec::new(),
            groups: Vec::new(),
            workspace: Workspace {
                id,
                name: name.into(),
                settings: Default::default(),
            },
            role: WorkspaceRole::Member,
            conversations: vec![self.channel(id, general, "general")],
            user_state: vec![ConversationUserState {
                conversation_id: general,
                favorite: false,
                last_read: EventSeq::ZERO,
                unread_count: unread,
                mention_count: mentions,
            }],
            members: vec![self.me.clone(), bob, self.carol.clone()],
            head: EventSeq::ZERO,
        }
    }

    /// Three workspaces: "Lazy Labs" (open; nothing needs attention), "Design Guild" (3
    /// mentions) and "Weekend Climbers" (unread messages and no mention).
    fn bootstrap(&self) -> BootstrapResponse {
        BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: self.me.clone(),
            workspaces: vec![
                self.workspace(self.ids[0], "Lazy Labs", self.general, 0, 0),
                self.workspace(
                    self.ids[1],
                    "Design Guild",
                    ConversationId::generate(),
                    9,
                    3,
                ),
                self.workspace(
                    self.ids[2],
                    "Weekend Climbers",
                    ConversationId::generate(),
                    6,
                    0,
                ),
            ],
        }
    }

    fn dispatch(&self, cx: &mut VisualTestContext, input: Input) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.run_until_parked();
    }

    fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.effects.0.borrow_mut())
    }

    fn core<R>(
        &self,
        cx: &VisualTestContext,
        read: impl FnOnce(&lazy_client_core::ClientCore) -> R,
    ) -> R {
        self.model.read_with(cx, |model, _| read(model.core()))
    }

    fn reach(&self, cx: &mut VisualTestContext) {
        cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(HEIGHT)));
        self.dispatch(
            cx,
            Input::Started(StartupState {
                session_restored: true,
                ..Default::default()
            }),
        );
        self.dispatch(cx, Input::BootstrapLoaded(Box::new(self.bootstrap())));
        self.dispatch(cx, Input::Clock(Timestamp::now()));
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
                    messages: vec![self.message(&self.bob, "hello from Bob", 1)],
                    has_more: false,
                }),
            },
        );
        draw(cx);
    }

    fn message(&self, author: &User, text: &str, seq: i64) -> Message {
        let at = time::OffsetDateTime::now_utc() - time::Duration::minutes(500 - seq);
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
            created_at: Timestamp::from(at),
            revision: 1,
            seq: EventSeq::new(seq).unwrap(),
            edited_at: None,
            reactions: vec![],
            reactions_as_of: EventSeq::ZERO,
            deleted: false,
        }
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

    /// Clicks in the box of a labelled field (the label sits above it).
    fn click_field(&self, cx: &mut VisualTestContext, selector: &'static str) {
        draw(cx);
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("nothing is labelled {selector:?} on screen"));
        let point = gpui::point(
            bounds.left() + gpui::px(40.),
            bounds.bottom() - gpui::px(20.),
        );
        cx.simulate_click(point, Modifiers::default());
        cx.run_until_parked();
    }

    fn shell(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::shell::ShellView> {
        self.root
            .read_with(cx, |root, _| root.shell().expect("the shell").clone())
    }

    fn rail(&self, cx: &mut VisualTestContext) -> Entity<RailView> {
        let shell = self.shell(cx);
        shell.read_with(cx, |shell, _| shell.rail().clone())
    }

    /// Gives the keyboard to the rail.
    fn focus_rail(&self, cx: &mut VisualTestContext) {
        let rail = self.rail(cx);
        let focus = rail.read_with(cx, |rail, _| rail.focus());
        cx.update(|window, cx| window.focus(&focus, cx));
        draw(cx);
    }

    fn rail_has_keyboard(&self, cx: &mut VisualTestContext) -> bool {
        let rail = self.rail(cx);
        cx.update(|window, cx| rail.read(cx).has_keyboard(window))
    }

    fn active(&self, cx: &VisualTestContext) -> Option<WorkspaceId> {
        self.core(cx, |core| core.active_workspace())
    }

    fn add_dialog(
        &self,
        cx: &mut VisualTestContext,
    ) -> Option<Entity<lazy_ui::dialogs::AddWorkspaceDialog>> {
        let shell = self.shell(cx);
        shell.read_with(cx, |shell, _| shell.add_workspace_dialog())
    }

    /// Opens "Add a workspace" with the rail's "+".
    fn open_add(&self, cx: &mut VisualTestContext) {
        self.click(cx, "rail-add");
        assert!(self.add_dialog(cx).is_some(), "the dialog opened");
    }

    fn persisted_workspaces(&self) -> Vec<Option<WorkspaceId>> {
        self.effects
            .0
            .borrow()
            .iter()
            .filter_map(|effect| match effect {
                Effect::PersistActiveWorkspace(id) => Some(*id),
                _ => None,
            })
            .collect()
    }
}

// ----- The rail -----

#[gpui::test]
fn the_rail_is_a_column_at_the_left_edge_with_a_square_per_workspace_and_a_plus(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    let rail = cx.debug_bounds("rail").expect("the rail");
    assert_eq!(rail.left(), gpui::px(0.), "at the window's left edge");
    assert_eq!(rail.size.width, gpui::px(RAIL_WIDTH));
    assert_eq!(rail.size.height, gpui::px(HEIGHT), "the window's height");
    let mut last_bottom = rail.top();
    for selector in ["rail-item-0", "rail-item-1", "rail-item-2", "rail-add"] {
        let square = cx.debug_bounds(selector).expect(selector);
        assert!(
            square.left() >= rail.left() && square.right() <= rail.right(),
            "{selector} is inside the rail"
        );
        assert!(
            square.top() >= last_bottom,
            "{selector} is below the one before it"
        );
        last_bottom = square.bottom();
    }
    assert!(
        cx.debug_bounds("rail-item-3").is_none(),
        "one square per workspace and no more"
    );
    // The sidebar starts where the rail ends.
    let preferences = cx
        .debug_bounds("sidebar-preferences")
        .expect("a sidebar link");
    assert!(
        preferences.left() >= rail.right(),
        "the sidebar is beside the rail"
    );
}

#[gpui::test]
fn the_rail_stays_when_the_window_is_as_small_as_it_gets_and_gets_slimmer(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    cx.simulate_resize(gpui::size(gpui::px(640.), gpui::px(480.)));
    draw(cx);
    let rail = cx.debug_bounds("rail").expect("the rail");
    assert_eq!(rail.size.width, gpui::px(COMPACT_RAIL_WIDTH));
    for selector in ["rail-item-0", "rail-item-1", "rail-item-2", "rail-add"] {
        let square = cx.debug_bounds(selector).expect(selector);
        assert!(
            square.bottom() <= gpui::px(480.) && square.right() <= rail.right(),
            "{selector} is wholly in the window"
        );
    }
    // The header's buttons are still all there: the rail did not push Details off the edge.
    let details = cx.debug_bounds("details-button").expect("Details");
    assert!(
        details.right() <= gpui::px(640.),
        "Details is inside the window: {details:?}"
    );
}

#[gpui::test]
fn a_square_shows_attention_as_a_badge_and_other_unread_as_a_dot(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    let rail = world.core(cx, |core| core.workspace_rail());
    assert_eq!(rail.len(), 3);
    assert_eq!(
        rail.iter()
            .map(|item| item.initials.as_str())
            .collect::<Vec<_>>(),
        ["LL", "DG", "WC"]
    );
    assert!(rail[0].active && !rail[1].active && !rail[2].active);
    assert_eq!(rail[1].attention, 3, "three mentions in Design Guild");
    assert!(
        rail[2].unread && rail[2].attention == 0,
        "Weekend Climbers has only unread messages"
    );
    // Nothing needs attention in the open workspace: no badge, and no dot on the open one.
    assert!(!world.on_screen(cx, "rail-badge-0") && !world.on_screen(cx, "rail-dot-0"));
    assert!(
        world.on_screen(cx, "rail-badge-1"),
        "a badge for the mentions"
    );
    assert!(
        !world.on_screen(cx, "rail-dot-1"),
        "a badge replaces the dot"
    );
    assert!(
        world.on_screen(cx, "rail-dot-2"),
        "a dot for the unread messages"
    );
    assert!(!world.on_screen(cx, "rail-badge-2"));
}

#[gpui::test]
fn what_a_screen_reader_and_the_tooltip_say_names_the_workspace_and_what_is_new(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    let rail = world.core(cx, |core| core.workspace_rail());
    let said: Vec<String> = rail.iter().map(RailView::describe).collect();
    assert!(said[0].starts_with("Lazy Labs") && said[0].contains("current workspace"));
    assert!(said[1].starts_with("Design Guild") && said[1].contains("3 need your attention"));
    assert!(!said[1].contains("current"));
    assert!(said[2].starts_with("Weekend Climbers") && said[2].contains("has unread messages"));
}

#[gpui::test]
fn clicking_a_square_switches_to_that_workspace(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    assert_eq!(world.active(cx), Some(world.ids[0]));
    world.take_effects();
    world.click(cx, "rail-item-1");
    assert_eq!(world.active(cx), Some(world.ids[1]));
    assert_eq!(
        world.persisted_workspaces().last(),
        Some(&Some(world.ids[1])),
        "the choice is saved"
    );
    let rail = world.core(cx, |core| core.workspace_rail());
    assert!(rail[1].active && !rail[0].active);
    world.click(cx, "rail-item-0");
    assert_eq!(world.active(cx), Some(world.ids[0]));
}

#[gpui::test]
fn the_rail_is_one_tab_stop_that_up_and_down_walk_and_enter_activates(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.focus_rail(cx);
    let rail = world.rail(cx);
    assert!(world.rail_has_keyboard(cx));
    assert_eq!(
        rail.read_with(cx, |rail, _| rail.cursor()),
        0,
        "on the open workspace"
    );
    cx.simulate_keystrokes("down");
    assert_eq!(rail.read_with(cx, |rail, _| rail.cursor()), 1);
    assert_eq!(
        world.active(cx),
        Some(world.ids[0]),
        "moving the cursor switches nothing"
    );
    cx.simulate_keystrokes("enter");
    assert_eq!(world.active(cx), Some(world.ids[1]));
    // As when a conversation is chosen in the sidebar, the keyboard goes on to the composer of
    // what was opened; Shift+Tab or a click brings it back to the rail, on the open workspace.
    assert!(
        !world.rail_has_keyboard(cx),
        "the composer has the keyboard"
    );
    world.focus_rail(cx);
    assert_eq!(rail.read_with(cx, |rail, _| rail.cursor()), 1);
    cx.simulate_keystrokes("down down");
    assert_eq!(
        rail.read_with(cx, |rail, _| rail.cursor()),
        3,
        "the + is the last stop, and Down stops there"
    );
    cx.simulate_keystrokes("down");
    assert_eq!(rail.read_with(cx, |rail, _| rail.cursor()), 3);
    cx.simulate_keystrokes("home");
    assert_eq!(rail.read_with(cx, |rail, _| rail.cursor()), 0);
    cx.simulate_keystrokes("up");
    assert_eq!(
        rail.read_with(cx, |rail, _| rail.cursor()),
        0,
        "Up stops at the top"
    );
    cx.simulate_keystrokes("end");
    assert_eq!(rail.read_with(cx, |rail, _| rail.cursor()), 3);
    cx.simulate_keystrokes("home space");
    assert_eq!(
        world.active(cx),
        Some(world.ids[0]),
        "Space activates like Enter"
    );
    // Tab leaves the rail in one step and Shift+Tab comes back to it.
    world.focus_rail(cx);
    cx.simulate_keystrokes("tab");
    assert!(!world.rail_has_keyboard(cx), "one Tab leaves the rail");
    cx.simulate_keystrokes("shift-tab");
    assert!(world.rail_has_keyboard(cx), "and Shift+Tab returns");
}

#[gpui::test]
fn the_cursor_starts_on_the_open_workspace_whenever_the_keyboard_arrives(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "rail-item-2");
    world.focus_rail(cx);
    let rail = world.rail(cx);
    assert_eq!(rail.read_with(cx, |rail, _| rail.cursor()), 2);
}

#[gpui::test]
fn ctrl_and_a_number_switches_from_anywhere_and_a_missing_number_does_nothing(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    // The keyboard is in the composer, as it is after opening a conversation.
    cx.simulate_keystrokes("ctrl-2");
    assert_eq!(world.active(cx), Some(world.ids[1]));
    cx.simulate_keystrokes("ctrl-3");
    assert_eq!(world.active(cx), Some(world.ids[2]));
    cx.simulate_keystrokes("ctrl-9");
    assert_eq!(world.active(cx), Some(world.ids[2]), "there is no ninth");
    cx.simulate_keystrokes("ctrl-1");
    assert_eq!(world.active(cx), Some(world.ids[0]));
}

#[gpui::test]
fn ctrl_and_a_number_does_nothing_over_a_dialog(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "sidebar-preferences");
    assert!(world.on_screen(cx, "prefs-scroll"));
    cx.simulate_keystrokes("ctrl-2");
    assert_eq!(
        world.active(cx),
        Some(world.ids[0]),
        "the dialog has the window"
    );
}

#[gpui::test]
fn switching_leaves_the_people_directory_of_the_workspace_that_was_left(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "sidebar-people");
    assert!(world.core(cx, |core| core.people().open));
    assert!(world.on_screen(cx, "people-view"));
    world.click(cx, "rail-item-1");
    assert_eq!(world.active(cx), Some(world.ids[1]));
    assert!(!world.core(cx, |core| core.people().open));
    assert!(
        !world.on_screen(cx, "people-view"),
        "the other workspace's people are not shown"
    );
}

#[gpui::test]
fn the_plus_opens_add_a_workspace_with_the_mouse_and_with_the_keyboard(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.open_add(cx);
    cx.simulate_keystrokes("escape");
    assert!(world.add_dialog(cx).is_none());
    // Back with the keyboard: End, Enter.
    world.focus_rail(cx);
    cx.simulate_keystrokes("end enter");
    assert!(
        world.add_dialog(cx).is_some(),
        "the + at the end of the rail opens it"
    );
    // Escape closes it and the keyboard is back where it came from: the rail.
    cx.simulate_keystrokes("escape");
    assert!(world.add_dialog(cx).is_none());
    assert!(world.rail_has_keyboard(cx), "focus returns to the rail");
}

// ----- Add a workspace -----

fn joined(world: &World, name: &str) -> (WorkspaceId, Box<WorkspaceBootstrap>) {
    let id = WorkspaceId::generate();
    let general = ConversationId::generate();
    (id, Box::new(world.workspace(id, name, general, 0, 0)))
}

#[gpui::test]
fn creating_a_workspace_shows_progress_a_refusal_and_then_arrives_as_the_open_one(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.take_effects();
    world.open_add(cx);
    let dialog = world.add_dialog(cx).unwrap();
    assert_eq!(dialog.read_with(cx, |dialog, _| dialog.way()), Way::Create);
    assert_eq!(
        cx.update(|window, cx| dialog.read(cx).stop(window, cx)),
        "name",
        "the name field has the keyboard"
    );
    // An empty name sends nothing.
    cx.simulate_keystrokes("enter");
    assert!(world.take_effects().is_empty());
    cx.simulate_input("Book Club");
    cx.simulate_keystrokes("enter");
    let sent: Vec<String> = world
        .take_effects()
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::CreateWorkspace { request, .. } => Some(request.name.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(sent, ["Book Club"], "one request, with the name as typed");
    assert_eq!(
        world.core(cx, |core| core.action(ActionKind::CreateWorkspace).clone()),
        ActionState::InFlight
    );
    // Pressing Enter again while it is in flight sends nothing more.
    cx.simulate_keystrokes("enter");
    assert!(world.take_effects().is_empty(), "not twice");
    assert!(world.on_screen(cx, "add-workspace-submit"), "still open");

    // Refused: the server's words, in place, with the name still there to fix.
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::CreateWorkspace,
            result: Err(RequestError::Rejected(ApiError::new(
                ErrorCode::Conflict,
                "You can only be in so many workspaces.",
            ))),
        },
    );
    assert!(world.on_screen(cx, "add-workspace-failed"));
    assert!(world.add_dialog(cx).is_some());
    cx.simulate_keystrokes("enter");
    let again = world
        .take_effects()
        .into_iter()
        .filter(|effect| matches!(effect, Effect::CreateWorkspace { .. }))
        .count();
    assert_eq!(again, 1, "the name was kept and sends again");

    // Created: the workspace is on the rail, open, and the dialog is gone.
    let (id, bootstrap) = joined(&world, "Book Club");
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::CreateWorkspace,
            result: Ok(ActionOutcome::Joined(bootstrap)),
        },
    );
    assert_eq!(world.active(cx), Some(id));
    assert_eq!(world.core(cx, |core| core.workspace_rail().len()), 4);
    assert!(world.add_dialog(cx).is_none(), "the dialog closed itself");
    assert!(world.on_screen(cx, "rail-item-3"), "a fourth square");
}

#[gpui::test]
fn joining_with_a_code_is_chosen_with_left_and_right_and_never_shows_the_code_back(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.take_effects();
    world.open_add(cx);
    let dialog = world.add_dialog(cx).unwrap();
    let stop =
        |cx: &mut VisualTestContext| cx.update(|window, cx| dialog.read(cx).stop(window, cx));
    // Shift+Tab to the choice; Right and Left choose, and the keyboard stays on the choice.
    cx.simulate_keystrokes("shift-tab");
    assert_eq!(stop(cx), "way");
    cx.simulate_keystrokes("right");
    assert_eq!(dialog.read_with(cx, |dialog, _| dialog.way()), Way::Join);
    assert_eq!(stop(cx), "way", "the keyboard stays on the choice");
    cx.simulate_keystrokes("left");
    assert_eq!(dialog.read_with(cx, |dialog, _| dialog.way()), Way::Create);
    cx.simulate_keystrokes("right tab");
    assert_eq!(stop(cx), "code");

    // A malformed code is refused here, without a request, and without repeating it.
    cx.simulate_input("abc");
    cx.simulate_keystrokes("enter");
    assert!(
        !world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::AcceptInvitation { .. })),
        "a code of the wrong shape goes nowhere"
    );
    let refusal = world.core(cx, |core| core.action(ActionKind::AcceptInvitation).clone());
    match refusal {
        ActionState::Failed(message) => {
            assert!(
                !message.contains("abc"),
                "the refusal does not echo the code"
            );
        }
        other => panic!("expected a refusal, found {other:?}"),
    }
    assert!(world.on_screen(cx, "add-workspace-failed"));

    // A well-formed one is sent, trimmed.
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input(" lazy-9f3kq2m7vx8d4ztb ");
    cx.simulate_keystrokes("enter");
    let codes: Vec<String> = world
        .take_effects()
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::AcceptInvitation { request } => Some(request.code.expose().to_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(codes, ["lazy-9f3kq2m7vx8d4ztb"]);
    assert_eq!(
        world.core(cx, |core| core.action(ActionKind::AcceptInvitation).clone()),
        ActionState::InFlight
    );

    // The server says no: its words show, the dialog stays, and the same code can be sent again.
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::AcceptInvitation,
            result: Err(RequestError::Rejected(ApiError::new(
                ErrorCode::NotFound,
                "That invitation isn't valid any more.",
            ))),
        },
    );
    assert!(world.on_screen(cx, "add-workspace-failed"));
    assert_eq!(dialog.read_with(cx, |dialog, _| dialog.way()), Way::Join);
    cx.simulate_keystrokes("enter");
    assert_eq!(
        world
            .take_effects()
            .iter()
            .filter(|effect| matches!(effect, Effect::AcceptInvitation { .. }))
            .count(),
        1,
        "the code stayed in the field and goes again"
    );

    // Joined: on the rail and open.
    let (id, bootstrap) = joined(&world, "Hiking Club");
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::AcceptInvitation,
            result: Ok(ActionOutcome::Joined(bootstrap)),
        },
    );
    assert_eq!(world.active(cx), Some(id));
    assert!(world.add_dialog(cx).is_none());
}

#[gpui::test]
fn closing_add_a_workspace_forgets_what_was_typed_and_any_refusal(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.take_effects();
    world.open_add(cx);
    cx.simulate_input("Half a nam");
    cx.simulate_keystrokes("escape");
    assert!(world.add_dialog(cx).is_none());
    world.open_add(cx);
    cx.simulate_keystrokes("enter");
    assert!(
        world.take_effects().is_empty(),
        "a reopened dialog starts empty, so Enter sends nothing"
    );
    // A refusal does not outlive the dialog either.
    cx.simulate_input("Zed");
    cx.simulate_keystrokes("enter");
    world.take_effects();
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::CreateWorkspace,
            result: Err(RequestError::Rejected(ApiError::new(
                ErrorCode::Conflict,
                "Nope.",
            ))),
        },
    );
    assert!(world.on_screen(cx, "add-workspace-failed"));
    cx.simulate_keystrokes("escape");
    world.open_add(cx);
    assert!(!world.on_screen(cx, "add-workspace-failed"));
}

#[test]
fn the_invitation_code_is_read_in_one_place_only() {
    // The dialog hands the code to the core and nothing else; anything that formats it (a label,
    // a tooltip, a log line) would leak it, so this reads the source and counts.
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/dialogs/add_workspace.rs"),
    )
    .expect("the dialog's source");
    // The typed text is read twice: whether the button is enabled and what is sent.
    assert_eq!(
        source.matches(".text()").count(),
        2,
        "the field's text is read in `can_submit` and `submit` and nowhere else"
    );
    assert_eq!(
        source.matches("format!").count()
            + source.matches("println!").count()
            + source.matches("eprintln!").count()
            + source.matches("tracing::").count(),
        0,
        "nothing in the dialog formats or logs text"
    );
}

// ----- Preferences: profile -----

#[gpui::test]
fn the_display_name_field_holds_the_current_name_and_saves_only_a_change(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.take_effects();
    world.click(cx, "sidebar-preferences");
    let shell = world.shell(cx);
    let dialog = shell
        .read_with(cx, |shell, _| shell.preferences_dialog())
        .expect("preferences");
    assert_eq!(
        dialog.read_with(cx, |dialog, cx| dialog.display_name_text(cx)),
        "Me Myself"
    );
    // Unchanged: nothing is sent, however it is asked.
    world.click(cx, "prefs-save-name");
    assert!(world.take_effects().is_empty());
    // Changed, saved with the button.
    world.click_field(cx, "prefs-display-name");
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input("Mia Myself");
    world.click(cx, "prefs-save-name");
    let sent: Vec<Option<String>> = world
        .take_effects()
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::UpdateProfile { request } => {
                Some(request.display_name.map(|name| name.to_string()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(sent, [Some("Mia Myself".to_owned())]);
    // The server refuses: its words show beside the button.
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::UpdateProfile,
            result: Err(RequestError::Rejected(ApiError::new(
                ErrorCode::InvalidRequest,
                "That name is taken.",
            ))),
        },
    );
    assert!(world.on_screen(cx, "prefs-name-failed"));
}

#[gpui::test]
fn a_blank_display_name_is_refused_on_the_spot_without_a_request(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.take_effects();
    world.click(cx, "sidebar-preferences");
    world.click_field(cx, "prefs-display-name");
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_keystrokes("enter");
    assert!(world.take_effects().is_empty());
    assert!(
        world.on_screen(cx, "field-error-Display name"),
        "the field says what is wrong"
    );
}

#[gpui::test]
fn choosing_a_picture_sends_it_and_removing_one_asks_to_remove_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.take_effects();
    world.click(cx, "sidebar-preferences");
    let shell = world.shell(cx);
    let dialog = shell
        .read_with(cx, |shell, _| shell.preferences_dialog())
        .expect("preferences");
    // The file dialog is the system's; its answer is handed in.
    dialog.update(cx, |dialog, cx| {
        dialog.picture_chosen("/home/me/face.png".into(), cx)
    });
    cx.run_until_parked();
    let sent: Vec<FileSource> = world
        .take_effects()
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::SetAvatar { source } => Some(source),
            _ => None,
        })
        .collect();
    assert_eq!(sent.len(), 1);
    assert!(matches!(&sent[0], FileSource::Path(path) if path.ends_with("face.png")));
    assert_eq!(
        world.core(cx, |core| core.action(ActionKind::SetAvatar).clone()),
        ActionState::InFlight
    );
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::SetAvatar,
            result: Err(RequestError::Rejected(ApiError::new(
                ErrorCode::InvalidRequest,
                "That picture is too large.",
            ))),
        },
    );
    assert!(world.on_screen(cx, "prefs-picture-failed"));
    // Nothing to remove while there is no picture: the button does nothing.
    world.click(cx, "prefs-remove-picture");
    assert!(
        !world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::RemoveAvatar)),
        "no picture, nothing to remove"
    );
}

#[gpui::test]
fn removing_a_picture_that_exists_asks_the_server(cx: &mut TestAppContext) {
    let (mut world, cx) = start(cx);
    world.me.avatar_version = 3;
    world.reach(cx);
    world.take_effects();
    world.click(cx, "sidebar-preferences");
    world.click(cx, "prefs-remove-picture");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::RemoveAvatar)),
        "Remove picture asks the server to remove it"
    );
}

// ----- Preferences: appearance -----

#[gpui::test]
fn choosing_a_theme_applies_it_at_once_and_is_saved(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.take_effects();
    world.click(cx, "sidebar-preferences");
    assert_eq!(cx.update(|_, cx| Theme::get(cx).mode), ThemeMode::Light);
    draw(cx);
    world.click(cx, "prefs-theme-dark");
    assert_eq!(
        world.core(cx, |core| core.appearance().theme),
        ThemeChoice::Dark
    );
    assert_eq!(
        cx.update(|_, cx| Theme::get(cx).mode),
        ThemeMode::Dark,
        "dark on a light system, at once"
    );
    assert!(
        world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::PersistAppearance(_))),
        "the choice is saved on this device"
    );
    world.click(cx, "prefs-theme-light");
    assert_eq!(cx.update(|_, cx| Theme::get(cx).mode), ThemeMode::Light);
    // Following the system again: the system here is light.
    world.click(cx, "prefs-theme-dark");
    world.click(cx, "prefs-theme-system");
    assert_eq!(cx.update(|_, cx| Theme::get(cx).mode), ThemeMode::Light);
    assert_eq!(
        world.core(cx, |core| core.appearance().theme),
        ThemeChoice::System
    );
}

#[gpui::test]
fn the_theme_choice_survives_the_system_changing_its_appearance(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.dispatch(
        cx,
        Input::SetAppearance(Appearance {
            theme: ThemeChoice::Light,
            motion: MotionChoice::System,
        }),
    );
    // The desktop reports the system going dark.
    cx.update(|_, cx| lazy_ui::theme::install(cx, WindowAppearance::Dark));
    assert_eq!(
        cx.update(|_, cx| Theme::get(cx).mode),
        ThemeMode::Light,
        "a chosen theme does not follow the system"
    );
    world.dispatch(
        cx,
        Input::SetAppearance(Appearance {
            theme: ThemeChoice::System,
            motion: MotionChoice::System,
        }),
    );
    assert_eq!(
        cx.update(|_, cx| Theme::get(cx).mode),
        ThemeMode::Dark,
        "following the system, it is dark now"
    );
}

#[gpui::test]
fn reduced_motion_is_applied_to_gpui_at_once(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "sidebar-preferences");
    assert!(
        !cx.update(|_, cx| cx.reduce_motion()),
        "motion is on by default"
    );
    // The keyboard walks down to the motion choice (the last control but Close), which scrolls
    // it into view; Right turns reduced motion on.
    cx.simulate_keystrokes(
        "tab tab tab tab tab tab tab tab tab tab tab tab tab tab tab tab tab tab",
    );
    cx.simulate_keystrokes("right");
    assert_eq!(
        world.core(cx, |core| core.appearance().motion),
        MotionChoice::Reduce
    );
    assert!(
        cx.update(|_, cx| cx.reduce_motion()),
        "On holds every animation still"
    );
    // And with the mouse, now that it is on screen.
    world.click(cx, "prefs-motion-off");
    assert_eq!(
        world.core(cx, |core| core.appearance().motion),
        MotionChoice::Full
    );
    assert!(!cx.update(|_, cx| cx.reduce_motion()));
    // "System" cannot read the system's setting (GPUI does not offer it), so it is full motion,
    // and the dialog says so.
    world.click(cx, "prefs-motion-on");
    assert!(cx.update(|_, cx| cx.reduce_motion()));
    world.click(cx, "prefs-motion-system");
    assert_eq!(
        world.core(cx, |core| core.appearance().motion),
        MotionChoice::System
    );
    assert!(!cx.update(|_, cx| cx.reduce_motion()));
    assert!(world.on_screen(cx, "prefs-motion-note"));
}

#[gpui::test]
fn the_appearance_choices_are_applied_when_the_app_opens(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    // The core restored the saved choices before the first screen (`StartupState`).
    world.dispatch(
        cx,
        Input::Started(StartupState {
            session_restored: true,
            appearance: Appearance {
                theme: ThemeChoice::Dark,
                motion: MotionChoice::Reduce,
            },
            ..Default::default()
        }),
    );
    world.dispatch(cx, Input::BootstrapLoaded(Box::new(world.bootstrap())));
    assert_eq!(cx.update(|_, cx| Theme::get(cx).mode), ThemeMode::Dark);
    assert!(cx.update(|_, cx| cx.reduce_motion()));
}

// ----- Pictures -----

#[gpui::test]
fn pictures_are_asked_for_only_for_people_on_screen_and_only_when_they_have_one(
    cx: &mut TestAppContext,
) {
    let (mut world, cx) = start(cx);
    world.bob_has_picture = true;
    world.carol.avatar_version = 2;
    world.reach(cx);
    draw(cx);
    let asked: Vec<UserId> = world
        .effects
        .0
        .borrow()
        .iter()
        .filter_map(|effect| match effect {
            Effect::FetchAvatar { user, .. } => Some(*user),
            _ => None,
        })
        .collect();
    assert!(
        asked.contains(&world.bob.id),
        "Bob wrote what is on screen and has a picture"
    );
    assert!(
        !asked.contains(&world.carol.id),
        "Carol has a picture but is nowhere on screen"
    );
    assert!(
        !asked.contains(&world.me.id),
        "no picture (version 0), nothing to ask for"
    );
}

#[gpui::test]
fn a_picture_that_arrives_fills_the_circle_without_moving_the_row(cx: &mut TestAppContext) {
    let (mut world, cx) = start(cx);
    world.bob_has_picture = true;
    world.reach(cx);
    let before = cx
        .debug_bounds("author-avatar-0")
        .expect("the author's circle");
    let key = ImageKey::Avatar(world.bob.id, 1);
    assert!(
        cx.update(|_, cx| ImageStore::global(cx).read(cx).get(&key).is_none()),
        "nothing decoded before the picture arrives"
    );
    world.dispatch(
        cx,
        Input::AvatarLoaded {
            user: world.bob.id,
            version: 1,
            result: Ok(ImageData::new(png())),
        },
    );
    // The decode happens off the render path; wait for it, with a bound.
    for _ in 0..200 {
        draw(cx);
        let ready = cx.update(|_, cx| {
            matches!(
                ImageStore::global(cx).read(cx).get(&key),
                Some(Slot::Ready(_))
            )
        });
        if ready {
            break;
        }
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(10));
    }
    assert!(
        cx.update(|_, cx| {
            matches!(
                ImageStore::global(cx).read(cx).get(&key),
                Some(Slot::Ready(_))
            )
        }),
        "the picture was decoded"
    );
    draw(cx);
    let after = cx.debug_bounds("author-avatar-0").expect("the circle");
    assert_eq!(before, after, "the circle neither moved nor changed size");
}

#[gpui::test]
fn a_person_without_a_picture_keeps_their_initial_and_nothing_is_asked(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    draw(cx);
    assert!(
        !world
            .effects
            .0
            .borrow()
            .iter()
            .any(|effect| matches!(effect, Effect::FetchAvatar { .. })),
        "nobody has a picture, so nothing is fetched"
    );
    assert!(
        cx.debug_bounds("author-avatar-0").is_some(),
        "the circle is there"
    );
}
