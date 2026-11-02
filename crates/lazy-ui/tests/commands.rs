//! Slice 3H on GPUI's test platform: the command registry's shortcuts, the command menu, the
//! quick switcher's `>` list, the shortcuts dialog, and the rules that keep a shortcut from
//! firing where it must not (another dialog, an input method's composition, a key a text field
//! needs). The real views and the real core run; a recording effect sink stands in for the
//! runtime.
//!
//! Every keyboard claim is made with real key events through GPUI's dispatch, every mouse claim
//! with real mouse events. A composition is simulated by the input handler's own calls, not by a
//! real input method (see the notes in `tests/messages.rs`).

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, EntityInputHandler as _, Modifiers, TestAppContext, VisualTestContext,
    WindowAppearance,
};
use lazy_client_core::{Effect, Input, StartupState};
use lazy_domain::notification::NotificationSettings;
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    MessageDocument, MessageId, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    BootstrapResponse, Conversation, ConversationUserState, HistoryPage, Message, PROTOCOL_VERSION,
    User, Workspace, WorkspaceBootstrap,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::commands::{self, Availability, CommandId, DialogState, Group};
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
    engineering: ConversationId,
    design: ConversationId,
    dm: ConversationId,
    role: WorkspaceRole,
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

fn start(cx: &mut TestAppContext, role: WorkspaceRole) -> (World, &mut VisualTestContext) {
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
        workspace: WorkspaceId::generate(),
        general: ConversationId::generate(),
        engineering: ConversationId::generate(),
        design: ConversationId::generate(),
        dm: ConversationId::generate(),
        role,
    };
    (world, cx)
}

/// Two frames: what is laid out in one is what the mouse can hit in the next.
fn draw(cx: &mut VisualTestContext) {
    for _ in 0..2 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
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
            member_count: 3,
        }
    }

    /// #design and the direct message have unread messages (design a mention too).
    fn bootstrap(&self) -> BootstrapResponse {
        let state = |id: ConversationId, unread: u32, mentions: u32| ConversationUserState {
            conversation_id: id,
            favorite: false,
            last_read: EventSeq::ZERO,
            unread_count: unread,
            mention_count: mentions,
        };
        BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: self.me.clone(),
            workspaces: vec![WorkspaceBootstrap {
                saved: Vec::new(),
                notification_settings: NotificationSettings::default(),
                conversation_notification_prefs: Vec::new(),
                custom_emoji: Vec::new(),
                threads: Vec::new(),
                groups: Vec::new(),
                workspace: Workspace {
                    id: self.workspace,
                    name: "Lazy Labs".into(),
                    settings: Default::default(),
                },
                role: self.role,
                conversations: vec![
                    self.channel(self.general, "general"),
                    self.channel(self.design, "design"),
                    self.channel(self.engineering, "engineering"),
                    Conversation {
                        kind: ConversationKind::Direct,
                        name: None,
                        participants: vec![self.me.id, self.bob.id],
                        ..self.channel(self.dm, "placeholder")
                    },
                ],
                user_state: vec![
                    state(self.general, 0, 0),
                    state(self.design, 5, 1),
                    state(self.engineering, 0, 0),
                    state(self.dm, 2, 0),
                ],
                members: vec![self.me.clone(), self.bob.clone()],
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

    fn core<R>(
        &self,
        cx: &VisualTestContext,
        read: impl FnOnce(&lazy_client_core::ClientCore) -> R,
    ) -> R {
        self.model.read_with(cx, |model, _| read(model.core()))
    }

    fn reach(&self, cx: &mut VisualTestContext) {
        cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(1600.)));
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
        let at = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
        self.dispatch(
            cx,
            Input::HistoryLoaded {
                conversation: self.general,
                generation,
                result: Ok(HistoryPage {
                    messages: vec![Message {
                        attachments: vec![],
                        mentions: Default::default(),
                        thread_root: None,
                        in_channel: false,
                        thread: None,
                        id: MessageId::generate(),
                        conversation_id: self.general,
                        author_id: self.bob.id,
                        document: MessageDocument::from_markdown("hello").unwrap(),
                        created_at: Timestamp::from(at),
                        revision: 1,
                        seq: EventSeq::new(1).unwrap(),
                        edited_at: None,
                        reactions: vec![],
                        reactions_as_of: EventSeq::ZERO,
                        rich: None,
                        deleted: false,
                    }],
                    has_more: false,
                }),
            },
        );
        draw(cx);
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

    fn shell(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::shell::ShellView> {
        self.root
            .read_with(cx, |root, _| root.shell().expect("the shell").clone())
    }

    fn selected(&self, cx: &VisualTestContext) -> Option<ConversationId> {
        self.core(cx, |core| core.selected_conversation())
    }

    fn snapshot(&self, cx: &mut VisualTestContext) -> commands::AppSnapshot {
        let shell = self.shell(cx);
        cx.update(|_, cx| shell.read(cx).snapshot(cx))
    }

    fn quick_switcher(
        &self,
        cx: &mut VisualTestContext,
    ) -> Option<Entity<lazy_ui::dialogs::QuickSwitcherDialog>> {
        let shell = self.shell(cx);
        shell.read_with(cx, |shell, _| shell.quick_switcher())
    }

    fn composer_input(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::text_input::TextInput> {
        let shell = self.shell(cx);
        shell.read_with(cx, |shell, cx| shell.composer().read(cx).input().clone())
    }

    /// Whether some dialog (the scrim) is up.
    fn dialog_up(&self, cx: &mut VisualTestContext) -> bool {
        self.on_screen(cx, "dialog-scrim")
    }

    /// Puts a composition in progress in the composer, as an input method would.
    fn compose(&self, cx: &mut VisualTestContext) {
        let input = self.composer_input(cx);
        input.update_in(cx, |input, window, cx| {
            input.replace_and_mark_text_in_range(None, "かな", None, window, cx)
        });
        assert!(input.read_with(cx, |input, _| input.is_composing()));
    }

    fn end_composition(&self, cx: &mut VisualTestContext) {
        let input = self.composer_input(cx);
        input.update_in(cx, |input, window, cx| input.unmark_text(window, cx));
        assert!(!input.read_with(cx, |input, _| input.is_composing()));
    }

    fn run(&self, cx: &mut VisualTestContext, id: CommandId) {
        let shell = self.shell(cx);
        shell.update_in(cx, |shell, window, cx| shell.run_command(id, window, cx));
        cx.run_until_parked();
    }
}

// ----- Every shortcut runs its command -----

macro_rules! shortcut_test {
    ($name:ident, $keys:expr, $check:expr) => {
        #[gpui::test]
        fn $name(cx: &mut TestAppContext) {
            let (world, cx) = start(cx, WorkspaceRole::Owner);
            world.reach(cx);
            let before = world.selected(cx);
            // The keyboard is in the composer, as it is after opening a conversation: the
            // shortcut works where the person types.
            cx.simulate_keystrokes($keys);
            draw(cx);
            let check: fn(&World, &mut VisualTestContext, Option<ConversationId>) -> bool = $check;
            assert!(
                check(&world, cx, before),
                "{} did not do its command",
                $keys
            );
            // It typed nothing into the composer.
            let text = world
                .composer_input(cx)
                .read_with(cx, |input, _| input.text().to_owned());
            assert_eq!(text, "", "{} put text in the composer", $keys);
        }
    };
}

shortcut_test!(ctrl_k_opens_the_quick_switcher, "ctrl-k", |w, cx, _| w
    .quick_switcher(cx)
    .is_some_and(|d| d.read_with(cx, |d, _| d.command_rows().is_empty())));
shortcut_test!(
    ctrl_shift_p_opens_the_command_list,
    "ctrl-shift-p",
    |w, cx, _| w
        .quick_switcher(cx)
        .is_some_and(|d| d.read_with(cx, |d, _| !d.command_rows().is_empty()))
);
shortcut_test!(ctrl_f_opens_search, "ctrl-f", |w, cx, _| w
    .core(cx, |core| core.search().is_some()));
shortcut_test!(ctrl_shift_t_opens_threads, "ctrl-shift-t", |w, cx, _| w
    .core(cx, |core| core.inbox().is_some()));
shortcut_test!(
    ctrl_shift_s_opens_saved_messages,
    "ctrl-shift-s",
    |w, cx, _| w.core(cx, |core| core.saved().open)
);
shortcut_test!(ctrl_shift_e_opens_people, "ctrl-shift-e", |w, cx, _| w
    .core(cx, |core| core.people().open));
shortcut_test!(ctrl_shift_l_browses_channels, "ctrl-shift-l", |w, cx, _| {
    let shell = w.shell(cx);
    shell.read_with(cx, |shell, _| shell.browse_dialog().is_some())
});
shortcut_test!(ctrl_n_starts_a_message, "ctrl-n", |w, cx, _| w
    .dialog_up(cx)
    && w.quick_switcher(cx).is_none());
shortcut_test!(ctrl_comma_opens_preferences, "ctrl-,", |w, cx, _| w
    .on_screen(cx, "prefs-scroll"));
shortcut_test!(ctrl_slash_opens_the_shortcuts, "ctrl-/", |w, cx, _| {
    let shell = w.shell(cx);
    shell.read_with(cx, |shell, _| shell.shortcuts_dialog().is_some())
});
shortcut_test!(
    alt_down_goes_to_the_next_conversation,
    "alt-down",
    |w, cx, before| { w.selected(cx) != before && w.selected(cx).is_some() }
);
shortcut_test!(
    alt_up_goes_to_the_previous_conversation,
    "alt-up",
    |w, cx, before| { w.selected(cx) != before && w.selected(cx).is_some() }
);
shortcut_test!(
    alt_shift_down_goes_to_the_next_unread,
    "alt-shift-down",
    |w, cx, _| { w.selected(cx) == Some(w.design) || w.selected(cx) == Some(w.dm) }
);
shortcut_test!(
    alt_shift_up_goes_to_the_previous_unread,
    "alt-shift-up",
    |w, cx, _| { w.selected(cx) == Some(w.design) || w.selected(cx) == Some(w.dm) }
);

#[gpui::test]
fn next_and_previous_walk_the_sidebar_in_its_order_and_wrap(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    let order: Vec<ConversationId> = world.core(cx, |core| {
        let sidebar = core.sidebar();
        sidebar
            .favorites
            .iter()
            .chain(&sidebar.channels)
            .chain(&sidebar.direct_and_group)
            .copied()
            .collect()
    });
    assert_eq!(order.len(), 4);
    let start_at = order
        .iter()
        .position(|id| Some(*id) == world.selected(cx))
        .expect("the open conversation is listed");
    cx.simulate_keystrokes("alt-down");
    assert_eq!(world.selected(cx), Some(order[(start_at + 1) % 4]));
    cx.simulate_keystrokes("alt-up alt-up");
    assert_eq!(world.selected(cx), Some(order[(start_at + 3) % 4]));
    // Round the whole list and back where it began.
    for _ in 0..4 {
        cx.simulate_keystrokes("alt-down");
    }
    assert_eq!(world.selected(cx), Some(order[(start_at + 3) % 4]));
    // The keyboard follows into the new conversation's composer.
    cx.simulate_input("x");
    let text = world
        .composer_input(cx)
        .read_with(cx, |input, _| input.text().to_owned());
    assert_eq!(text, "x");
}

#[gpui::test]
fn next_unread_skips_what_has_been_read_and_never_stays_put(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    let mut visited = Vec::new();
    for _ in 0..4 {
        cx.simulate_keystrokes("alt-shift-down");
        let now = world.selected(cx).unwrap();
        assert!(
            now == world.design || now == world.dm,
            "only the two unread conversations are visited"
        );
        visited.push(now);
    }
    assert_ne!(visited[0], visited[1], "each press moves on");
    assert_eq!(visited[0], visited[2], "and it wraps");
}

// ----- Where a shortcut must not run -----

#[gpui::test]
fn a_shortcut_does_nothing_over_another_dialog(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    world.click(cx, "sidebar-preferences");
    assert!(world.on_screen(cx, "prefs-scroll"));
    let before = world.selected(cx);
    for keys in [
        "ctrl-shift-t",
        "ctrl-shift-e",
        "ctrl-shift-s",
        "ctrl-f",
        "ctrl-shift-p",
        "ctrl-/",
        "alt-down",
        "alt-shift-down",
        "ctrl-n",
        "ctrl-shift-l",
    ] {
        cx.simulate_keystrokes(keys);
        draw(cx);
        assert!(
            world.on_screen(cx, "prefs-scroll"),
            "{keys} closed or replaced the dialog"
        );
        assert!(world.core(cx, |core| core.inbox().is_none()), "{keys}");
        assert!(world.core(cx, |core| core.search().is_none()), "{keys}");
        assert!(!world.core(cx, |core| core.people().open), "{keys}");
        assert!(!world.core(cx, |core| core.saved().open), "{keys}");
        assert_eq!(world.selected(cx), before, "{keys} moved the conversation");
        assert!(world.quick_switcher(cx).is_none(), "{keys}");
    }
    let snapshot = world.snapshot(cx);
    assert_eq!(snapshot.dialog, DialogState::Own(CommandId::Preferences));
    assert!(matches!(
        commands::availability(CommandId::Search, &snapshot),
        Availability::Disabled(_)
    ));
}

#[gpui::test]
fn a_dialogs_own_shortcut_closes_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    cx.simulate_keystrokes("ctrl-,");
    assert!(world.on_screen(cx, "prefs-scroll"));
    cx.simulate_keystrokes("ctrl-,");
    assert!(
        !world.on_screen(cx, "prefs-scroll"),
        "the shortcut that opened it closes it"
    );
    cx.simulate_keystrokes("ctrl-/");
    assert!(
        world
            .shell(cx)
            .read_with(cx, |s, _| s.shortcuts_dialog().is_some())
    );
    cx.simulate_keystrokes("ctrl-/");
    assert!(
        world
            .shell(cx)
            .read_with(cx, |s, _| s.shortcuts_dialog().is_none())
    );
    // And Ctrl+K inside the switcher, as it always did.
    cx.simulate_keystrokes("ctrl-k");
    assert!(world.quick_switcher(cx).is_some());
    cx.simulate_keystrokes("ctrl-k");
    assert!(world.quick_switcher(cx).is_none());
}

#[gpui::test]
fn no_shortcut_runs_while_an_input_method_has_text_in_composition(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    world.compose(cx);
    let before = world.selected(cx);
    for command in commands::all() {
        let Some(spec) = command.shortcut else {
            continue;
        };
        let keys = commands::keystroke(spec);
        cx.simulate_keystrokes(&keys);
        draw(cx);
        assert!(
            !world.dialog_up(cx),
            "{keys} opened a dialog during composition"
        );
        assert!(world.core(cx, |core| core.search().is_none()), "{keys}");
        assert!(world.core(cx, |core| core.inbox().is_none()), "{keys}");
        assert!(!world.core(cx, |core| core.people().open), "{keys}");
        assert!(!world.core(cx, |core| core.saved().open), "{keys}");
        assert_eq!(world.selected(cx), before, "{keys} moved the conversation");
    }
    let snapshot = world.snapshot(cx);
    assert!(snapshot.composing);
    assert!(matches!(
        commands::availability(CommandId::QuickSwitcher, &snapshot),
        Availability::Disabled(_)
    ));
    // Run directly (the menu, a button) it is refused for the same reason.
    world.run(cx, CommandId::Preferences);
    assert!(!world.dialog_up(cx));
    // Once the composition has ended the same keys work.
    world.end_composition(cx);
    cx.simulate_keystrokes("ctrl-k");
    assert!(world.quick_switcher(cx).is_some());
}

#[test]
fn no_shortcut_is_a_key_a_text_input_uses_for_editing() {
    for mac in [false, true] {
        let editing = lazy_ui::text_input::editing_keystrokes_for(mac);
        assert!(editing.len() > 20, "the table was read");
        for command in commands::all() {
            let Some(spec) = command.shortcut else {
                continue;
            };
            let text = spec
                .split('-')
                .map(|part| match (part, mac) {
                    ("secondary", true) => "cmd",
                    ("secondary", false) => "ctrl",
                    (other, _) => other,
                })
                .collect::<Vec<_>>()
                .join("-");
            let text: &'static str = Box::leak(text.into_boxed_str());
            let keystroke = gpui::Keystroke::parse(text).unwrap();
            assert!(
                !editing
                    .iter()
                    .any(|edit| edit.modifiers == keystroke.modifiers && edit.key == keystroke.key),
                "{text} is a key the text input needs (mac: {mac})"
            );
        }
    }
}

#[gpui::test]
fn the_shortcuts_type_nothing_into_a_text_field_that_has_text(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    cx.simulate_input("draft");
    for keys in [
        "alt-up",
        "alt-down",
        "alt-shift-up",
        "alt-shift-down",
        "ctrl-/",
        "ctrl-,",
    ] {
        cx.simulate_keystrokes(keys);
        draw(cx);
        // Close whatever opened, and go back to the first conversation's composer.
        cx.simulate_keystrokes("escape");
    }
    // The draft belongs to the conversation that was open; every other composer is empty and
    // this one still holds what was typed.
    while world.selected(cx) != Some(world.general) {
        cx.simulate_keystrokes("alt-down");
    }
    let text = world
        .composer_input(cx)
        .read_with(cx, |input, _| input.text().to_owned());
    assert_eq!(text, "draft", "no shortcut inserted or removed a character");
}

// ----- The command menu -----

#[gpui::test]
fn the_menu_button_opens_the_registrys_groups_as_tabs(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    assert!(
        world.on_screen(cx, "sidebar-menu"),
        "the button is at the top of the sidebar"
    );
    world.click(cx, "sidebar-menu");
    let shell = world.shell(cx);
    let menu = shell
        .read_with(cx, |shell, _| shell.command_menu().cloned())
        .expect("the menu opened");
    assert!(world.on_screen(cx, "command-menu"));
    for group in Group::ALL {
        let selector: &'static str =
            Box::leak(format!("command-menu-tab-{}", group.title()).into_boxed_str());
        assert!(world.on_screen(cx, selector), "a tab for {}", group.title());
    }
    assert_eq!(
        menu.read_with(cx, |menu, _| menu.open_tab().map(str::to_owned)),
        Some("File".to_owned())
    );
    // Right and Left change the tab, wrapping.
    for expected in ["Go", "View", "Workspace", "Help", "File"] {
        cx.simulate_keystrokes("right");
        assert_eq!(
            menu.read_with(cx, |menu, _| menu.open_tab().map(str::to_owned)),
            Some(expected.to_owned())
        );
    }
    cx.simulate_keystrokes("left");
    assert_eq!(
        menu.read_with(cx, |menu, _| menu.open_tab().map(str::to_owned)),
        Some("Help".to_owned())
    );
}

#[gpui::test]
fn the_menu_is_operated_with_the_keyboard_and_runs_the_command(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    world.click(cx, "sidebar-menu");
    let shell = world.shell(cx);
    let menu = shell
        .read_with(cx, |shell, _| shell.command_menu().cloned())
        .unwrap();
    // File: Down to Preferences, then Enter.
    for _ in 0..8 {
        if menu.read_with(cx, |menu, _| menu.highlighted_id() == Some("Preferences")) {
            break;
        }
        cx.simulate_keystrokes("down");
    }
    assert_eq!(
        menu.read_with(cx, |menu, _| menu.highlighted_id().map(str::to_owned)),
        Some("Preferences".to_owned())
    );
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert!(
        world.on_screen(cx, "prefs-scroll"),
        "Enter ran Preferences…"
    );
    assert!(shell.read_with(cx, |shell, _| shell.command_menu().is_none()));
}

#[gpui::test]
fn escape_closes_the_menu_and_gives_the_keyboard_back_to_its_button(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    world.click(cx, "sidebar-menu");
    let shell = world.shell(cx);
    assert!(shell.read_with(cx, |shell, _| shell.command_menu().is_some()));
    cx.simulate_keystrokes("escape");
    assert!(shell.read_with(cx, |shell, _| shell.command_menu().is_none()));
    // The button has the keyboard again: Enter opens the menu once more.
    cx.simulate_keystrokes("enter");
    assert!(shell.read_with(cx, |shell, _| shell.command_menu().is_some()));
    // A shortcut does nothing while the menu is open.
    cx.simulate_keystrokes("ctrl-shift-t");
    assert!(world.core(cx, |core| core.inbox().is_none()));
}

#[gpui::test]
fn a_menu_item_can_be_clicked_and_a_tab_too(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    world.click(cx, "sidebar-menu");
    world.click(cx, "command-menu-tab-Go");
    world.click(cx, "command-menu-Threads");
    draw(cx);
    assert!(
        world.core(cx, |core| core.inbox().is_some()),
        "the click ran Threads"
    );
}

#[gpui::test]
fn what_cannot_run_is_dimmed_with_its_reason_and_does_nothing(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Member);
    world.reach(cx);
    world.click(cx, "sidebar-menu");
    let shell = world.shell(cx);
    let menu = shell
        .read_with(cx, |shell, _| shell.command_menu().cloned())
        .unwrap();
    world.click(cx, "command-menu-tab-Workspace");
    let items: Vec<(String, bool, Option<String>)> = menu.read_with(cx, |menu, _| {
        menu.items()
            .iter()
            .map(|item| {
                (
                    item.label.to_string(),
                    item.enabled,
                    item.detail.as_ref().map(|detail| detail.to_string()),
                )
            })
            .collect()
    });
    assert!(items.len() >= 4, "{items:?}");
    for (label, enabled, detail) in &items {
        assert!(!enabled, "{label} is for admins and owners");
        assert!(
            detail
                .as_deref()
                .is_some_and(|reason| reason.contains("admins and owners")),
            "{label} says why: {detail:?}"
        );
    }
    // Enter on a dimmed item runs nothing.
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert!(!world.dialog_up(cx));
    // The File tab's "Create a channel…" is open to everyone under the default policy.
    world.click(cx, "command-menu-tab-File");
    let create = menu.read_with(cx, |menu, _| {
        menu.items()
            .iter()
            .find(|item| item.label.starts_with("Create a channel"))
            .map(|item| item.enabled)
    });
    assert_eq!(create, Some(true));
}

#[gpui::test]
fn the_menu_shows_each_commands_shortcut_beside_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    world.click(cx, "sidebar-menu");
    let shell = world.shell(cx);
    let menu = shell
        .read_with(cx, |shell, _| shell.command_menu().cloned())
        .unwrap();
    world.click(cx, "command-menu-tab-Go");
    let shortcuts: Vec<Option<String>> = menu.read_with(cx, |menu, _| {
        menu.items()
            .iter()
            .map(|item| item.shortcut.as_ref().map(|s| s.to_string()))
            .collect()
    });
    assert!(
        shortcuts.contains(&Some("Ctrl+K".to_owned())),
        "{shortcuts:?}"
    );
    assert!(
        shortcuts.contains(&Some("Ctrl+Shift+T".to_owned())),
        "{shortcuts:?}"
    );
}

// ----- The quick switcher's `>` list -----

#[gpui::test]
fn a_greater_than_sign_lists_the_commands_that_can_run(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Member);
    world.reach(cx);
    cx.simulate_keystrokes("ctrl-k");
    cx.simulate_input(">");
    draw(cx);
    let dialog = world.quick_switcher(cx).unwrap();
    let rows: Vec<(CommandId, Option<String>)> = dialog.read_with(cx, |d, _| {
        d.command_rows()
            .iter()
            .map(|row| (row.id, row.shortcut.clone()))
            .collect()
    });
    assert!(rows.iter().any(|(id, _)| *id == CommandId::Preferences));
    assert!(
        rows.iter()
            .any(|(id, shortcut)| *id == CommandId::QuickSwitcher
                && shortcut.as_deref() == Some("Ctrl+K")),
        "each row has its shortcut"
    );
    assert!(
        !rows
            .iter()
            .any(|(id, _)| *id == CommandId::WorkspaceSettings),
        "a member cannot open workspace settings, so it is not listed"
    );
    assert!(world.on_screen(cx, "quick-command-0"));
    // Typing narrows it, fuzzily.
    cx.simulate_input("prf");
    let rows: Vec<CommandId> = dialog.read_with(cx, |d, _| {
        d.command_rows().iter().map(|row| row.id).collect()
    });
    assert_eq!(rows.first(), Some(&CommandId::Preferences));
    // Enter runs the highlighted command once the switcher has closed.
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert!(world.quick_switcher(cx).is_none());
    assert!(world.on_screen(cx, "prefs-scroll"), "Preferences… ran");
}

#[gpui::test]
fn the_palette_for_an_owner_lists_the_admin_commands_and_a_miss_says_so(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    cx.simulate_keystrokes("ctrl-shift-p");
    let dialog = world.quick_switcher(cx).unwrap();
    let ids: Vec<CommandId> =
        dialog.read_with(cx, |d, _| d.command_rows().iter().map(|r| r.id).collect());
    assert!(ids.contains(&CommandId::WorkspaceSettings));
    assert!(ids.contains(&CommandId::Diagnostics));
    cx.simulate_input("zzzqq");
    assert!(world.on_screen(cx, "quick-empty"));
    // Escape closes it, and the keyboard is back in the composer.
    cx.simulate_keystrokes("escape");
    assert!(world.quick_switcher(cx).is_none());
    cx.simulate_input("x");
    let text = world
        .composer_input(cx)
        .read_with(cx, |input, _| input.text().to_owned());
    assert_eq!(text, "x");
}

#[gpui::test]
fn a_command_chosen_with_the_mouse_runs(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    cx.simulate_keystrokes("ctrl-shift-p");
    cx.simulate_input("diag");
    world.click(cx, "quick-command-0");
    draw(cx);
    let shell = world.shell(cx);
    assert!(shell.read_with(cx, |shell, _| shell.diagnostics_dialog().is_some()));
}

// ----- The shortcuts dialog -----

#[gpui::test]
fn the_shortcuts_dialog_opens_with_the_keyboard_on_its_list_and_gives_it_back(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    cx.simulate_keystrokes("ctrl-/");
    assert!(world.on_screen(cx, "shortcuts-list"));
    // Down and Up scroll the list (it has the keyboard); Tab stays inside the dialog.
    cx.simulate_keystrokes("down down tab shift-tab");
    assert!(world.on_screen(cx, "shortcuts-list"));
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "shortcuts-list"));
    cx.simulate_input("y");
    let text = world
        .composer_input(cx)
        .read_with(cx, |input, _| input.text().to_owned());
    assert_eq!(text, "y", "the keyboard went back to the composer");
}

// ----- Buttons and the registry agree -----

#[gpui::test]
fn the_sidebars_buttons_ask_the_registry_so_a_dialog_blocks_them_like_a_shortcut(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    // Run directly with a dialog up, as a stray click or a menu would.
    world.click(cx, "sidebar-preferences");
    world.run(cx, CommandId::Threads);
    world.run(cx, CommandId::People);
    assert!(world.core(cx, |core| core.inbox().is_none() && !core.people().open));
    cx.simulate_keystrokes("escape");
    // And with nothing up they run.
    world.run(cx, CommandId::People);
    assert!(world.core(cx, |core| core.people().open));
}

#[gpui::test]
fn sign_out_goes_through_the_registry(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    world.run(cx, CommandId::SignOut);
    assert!(
        world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::SignOut)),
        "signing out asks the runtime to forget the session"
    );
}

#[gpui::test]
fn a_command_that_needs_a_conversation_runs_on_the_open_one(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    world.run(cx, CommandId::ConversationDetails);
    draw(cx);
    assert!(world.dialog_up(cx), "Details opened");
    cx.simulate_keystrokes("escape");
    world.take_effects();
    world.run(cx, CommandId::ToggleFavorite);
    let favorited = world
        .take_effects()
        .into_iter()
        .any(|effect| matches!(effect, Effect::SetFavorite { favorite: true, .. }));
    assert!(
        favorited,
        "Favorite sends the same request as the header's button"
    );
    // With another view in the conversation's place there is nothing to show details of.
    world.run(cx, CommandId::People);
    assert!(matches!(
        commands::availability(CommandId::ConversationDetails, &world.snapshot(cx)),
        Availability::Disabled(_)
    ));
}

#[gpui::test]
fn the_sidebars_sign_out_the_headers_buttons_and_the_plus_menu_all_go_through_the_registry(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    world.take_effects();
    // Favorite and Details in the conversation's header.
    world.click(cx, "favorite-toggle");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::SetFavorite { favorite: true, .. })),
        "the header's Favorite is the registry's command"
    );
    world.click(cx, "details-button");
    draw(cx);
    assert!(
        world.dialog_up(cx),
        "the header's Details is the registry's command"
    );
    cx.simulate_keystrokes("escape");
    // The sidebar's own Sign out.
    world.click(cx, "sign-out");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::SignOut)),
        "the sidebar's Sign out is the registry's command"
    );
}

#[gpui::test]
fn the_create_channel_command_is_open_to_members_under_the_default_policy(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Member);
    world.reach(cx);
    let snapshot = world.snapshot(cx);
    // The default policy lets members create channels, so the command is open to them.
    assert_eq!(
        commands::availability(CommandId::CreateChannel, &snapshot),
        Availability::Enabled
    );
}
