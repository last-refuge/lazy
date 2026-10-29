//! The user groups dialog on GPUI's test platform: who sees the entry, the list, the editor with
//! live handle checks, the people picker, deleting with a confirmation, and what happens to focus.
//! The real views and the real core run; a recording effect sink stands in for the runtime.
//!
//! Every keyboard claim is made with real key events through GPUI's dispatch. The test platform
//! never reports a window as active and gives every character the same width, so these prove
//! behavior and wiring; real layout is checked in the offscreen renders. Nothing here exercises a
//! real input method, a screen reader, or a display.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, Modifiers, TestAppContext, VisualTestContext, WindowAppearance,
};
use lazy_client_core::{
    ActionKind, ActionOutcome, ActionState, Effect, Input, RequestError, StartupState,
};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    GroupHandle, GroupId, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    ApiError, BootstrapResponse, Conversation, ConversationUserState, ErrorCode, Event,
    EventEnvelope, GroupResponse, HistoryPage, PROTOCOL_VERSION, ServerFrame, User, UserGroup,
    Workspace, WorkspaceBootstrap,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::commands::{AppSnapshot, CommandId, availability};
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
    zoe: User,
    workspace: WorkspaceId,
    general: ConversationId,
    /// Listed second (groups are ordered by handle).
    team: UserGroup,
    /// Listed first.
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
    let (me, bob, zoe) = (person("alice"), person("bob"), person("zoe"));
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
        workspace,
        general: ConversationId::generate(),
        team,
        ops,
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
                groups: vec![self.team.clone(), self.ops.clone()],
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

    fn dispatch(&self, cx: &mut VisualTestContext, input: Input) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.run_until_parked();
    }

    fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.effects.0.borrow_mut())
    }

    fn reach_as(&self, cx: &mut VisualTestContext, role: WorkspaceRole) {
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
                    messages: vec![],
                    has_more: false,
                }),
            },
        );
        draw(cx);
        self.take_effects();
    }

    fn admin(&self, cx: &mut VisualTestContext) {
        self.reach_as(cx, WorkspaceRole::Admin);
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

    /// Opens the dialog the way a person does: Menu, Workspace, User groups.
    fn open_dialog(&self, cx: &mut VisualTestContext) {
        self.click(cx, "sidebar-menu");
        self.click(cx, "command-menu-tab-Workspace");
        self.click(cx, "command-menu-UserGroups");
        assert!(self.on_screen(cx, "groups-list"), "the list dialog is open");
    }

    /// Opens the dialog where the keyboard is, without going through the Menu (which would take
    /// the keyboard and give it back to its own button first).
    fn open_in_place(&self, cx: &mut VisualTestContext) {
        self.root.update_in(cx, |root, window, cx| {
            root.shell().unwrap().update(cx, |shell, cx| {
                shell.open_dialog(ShellDialog::Groups, window, cx)
            });
        });
        assert!(self.on_screen(cx, "groups-list"), "the list dialog is open");
    }

    fn state(&self, cx: &VisualTestContext, kind: ActionKind) -> ActionState {
        self.model
            .read_with(cx, |m, _| m.core().action(kind).clone())
    }

    fn groups(&self, cx: &VisualTestContext) -> Vec<(String, String, usize)> {
        self.model.read_with(cx, |m, _| {
            m.core()
                .groups()
                .iter()
                .map(|g| (g.handle.to_string(), g.name.to_string(), g.members.len()))
                .collect()
        })
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

    /// A control in the sidebar to give the keyboard to before opening the dialog: the person's
    /// own row. The dialog hands the keyboard back to whatever had it.
    fn groups_focus(&self, cx: &mut VisualTestContext) -> gpui::FocusHandle {
        self.root.read_with(cx, |root, cx| {
            root.shell()
                .unwrap()
                .read(cx)
                .sidebar()
                .read(cx)
                .prefs_focus()
        })
    }

    fn create_requests(&self) -> Vec<lazy_protocol::CreateGroupRequest> {
        self.take_effects()
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::CreateGroup { request, .. } => Some(request),
                _ => None,
            })
            .collect()
    }
}

// ---- The entry point --------------------------------------------------------------------------------

#[gpui::test]
fn only_admins_and_owners_can_run_the_command(cx: &mut TestAppContext) {
    for (role, expected) in [
        (WorkspaceRole::Member, false),
        (WorkspaceRole::Admin, true),
        (WorkspaceRole::Owner, true),
    ] {
        let (world, cx) = start(cx);
        world.reach_as(cx, role);
        let snapshot = world
            .model
            .read_with(cx, |model, _| AppSnapshot::from_core(model.core()));
        assert_eq!(
            availability(CommandId::UserGroups, &snapshot).is_enabled(),
            expected,
            "{role:?}"
        );
        assert!(
            !world.on_screen(cx, "sidebar-user-groups"),
            "the sidebar has no link: the command is in the Menu"
        );
    }
}

#[gpui::test]
fn a_member_cannot_open_the_dialog_by_any_route(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Member);
    world.root.update_in(cx, |root, window, cx| {
        root.shell().unwrap().update(cx, |shell, cx| {
            shell.open_dialog(ShellDialog::Groups, window, cx)
        });
    });
    assert!(
        !world.on_screen(cx, "groups-list"),
        "members see groups only as things to mention"
    );
}

// ---- The list ---------------------------------------------------------------------------------------

#[gpui::test]
fn the_list_shows_every_group_and_the_keyboard_chooses_one(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.admin(cx);
    world.open_dialog(cx);
    assert!(world.on_screen(cx, "group-row-0"), "ops");
    assert!(world.on_screen(cx, "group-row-1"), "team");
    assert!(!world.on_screen(cx, "group-row-2"));

    // The list has the keyboard from the start: Down chooses "team", Enter opens it, and the
    // editor shows its handle.
    cx.simulate_keystrokes("down enter");
    assert!(world.on_screen(cx, "group-save"), "the editor is showing");
    cx.simulate_keystrokes("ctrl-a ctrl-c");
    assert_eq!(
        clipboard(cx).as_deref(),
        Some("team"),
        "focus starts on the handle"
    );
}

#[gpui::test]
fn an_empty_list_starts_on_new_group_and_has_no_stop_of_its_own(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.admin(cx);
    for (seq, group) in [(1, &world.team), (2, &world.ops)] {
        world.event(cx, seq, Event::GroupDeleted { group_id: group.id });
    }
    world.open_dialog(cx);
    assert!(!world.on_screen(cx, "group-row-0"));
    assert!(
        world.on_screen(cx, "groups-new"),
        "New group is still there"
    );

    // Nothing to choose, so focus starts on New group: Enter goes straight to the editor.
    cx.simulate_keystrokes("enter");
    assert!(world.on_screen(cx, "group-save"), "the editor opened");
}

// ---- Creating ---------------------------------------------------------------------------------------

#[gpui::test]
fn creating_a_group_checks_the_handle_as_it_is_typed_and_saves_what_was_chosen(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.admin(cx);
    world.open_dialog(cx);
    world.click(cx, "groups-new");
    assert!(world.on_screen(cx, "group-save"));
    assert!(
        world.on_screen(cx, "field-hint-Handle"),
        "an empty handle gets guidance, not an error"
    );
    assert!(!world.on_screen(cx, "field-error-Handle"));

    // Save does nothing while the form is incomplete.
    world.click(cx, "group-save");
    assert!(world.create_requests().is_empty());
    // Clicking Save focused it; go back to the handle (Save, Cancel, people, name, handle).
    cx.simulate_keystrokes("shift-tab shift-tab shift-tab shift-tab");

    // Each rule is explained as soon as it is broken, and Save stays out of reach.
    for bad in ["x", "channel", "has space", "-lead", "café"] {
        cx.simulate_keystrokes("ctrl-a backspace");
        cx.simulate_input(bad);
        assert!(
            world.on_screen(cx, "field-error-Handle"),
            "{bad:?} is explained"
        );
    }
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input("@Design-2");
    assert!(!world.on_screen(cx, "field-error-Handle"));
    assert!(
        world.on_screen(cx, "field-hint-Handle"),
        "a usable handle shows what it will be saved as"
    );

    // Name, then people.
    cx.simulate_keystrokes("tab");
    cx.simulate_input("Designers");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("bo");
    cx.simulate_keystrokes("enter");
    cx.simulate_input("zo");
    cx.simulate_keystrokes("enter");
    assert!(world.create_requests().is_empty(), "nothing is sent yet");

    // Enter in the people box with nothing typed saves.
    cx.simulate_keystrokes("enter");
    let requests = world.create_requests();
    assert_eq!(requests.len(), 1, "one create request");
    let request = &requests[0];
    assert_eq!(
        request.handle.as_str(),
        "design-2",
        "@ and capitals are forgiven"
    );
    assert_eq!(request.name.as_str(), "Designers");
    assert_eq!(request.members, vec![world.bob.id, world.zoe.id]);
    assert_eq!(
        world.state(cx, ActionKind::CreateGroup),
        ActionState::InFlight
    );

    // The server's answer returns to the list, which now has the new group.
    let created = UserGroup {
        id: GroupId::generate(),
        workspace_id: world.workspace,
        handle: request.handle.clone(),
        name: request.name.clone(),
        members: request.members.clone(),
    };
    world.dispatch(
        cx,
        Input::GroupCommandResult {
            operation: request.operation_id,
            result: Ok(GroupResponse { group: created }),
        },
    );
    assert!(world.on_screen(cx, "groups-list"), "back at the list");
    assert!(!world.on_screen(cx, "group-save"));
    assert_eq!(world.groups(cx).len(), 3);
    assert!(world.on_screen(cx, "group-row-2"));
}

#[gpui::test]
fn a_refusal_stays_on_screen_and_the_form_can_be_fixed_and_saved_again(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.admin(cx);
    world.open_dialog(cx);
    world.click(cx, "groups-new");

    // "bob" is a person's username: the core refuses the handle.
    cx.simulate_input("bob");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("Bobs");
    cx.simulate_keystrokes("enter");
    assert!(
        world.create_requests().is_empty(),
        "the core never asked the server"
    );
    assert!(
        matches!(
            world.state(cx, ActionKind::CreateGroup),
            ActionState::Failed(_)
        ),
        "the core explains it"
    );
    assert!(
        world.on_screen(cx, "group-error"),
        "and the dialog shows that"
    );
    assert!(world.on_screen(cx, "group-save"), "the form is still open");

    // A different handle goes through; the server then refuses for its own reason.
    cx.simulate_keystrokes("shift-tab ctrl-a backspace");
    cx.simulate_input("bobs-group");
    cx.simulate_keystrokes("enter");
    let requests = world.create_requests();
    assert_eq!(requests.len(), 1, "sent after the fix");
    assert!(
        !world.on_screen(cx, "group-error"),
        "starting again clears the old message"
    );
    world.dispatch(
        cx,
        Input::GroupCommandResult {
            operation: requests[0].operation_id,
            result: Err(RequestError::Rejected(ApiError::new(
                ErrorCode::LimitReached,
                "This workspace has the most groups it can have.",
            ))),
        },
    );
    assert!(
        world.on_screen(cx, "group-error"),
        "the server's reason is shown"
    );
    assert!(
        world.on_screen(cx, "group-save"),
        "and the form stays open to retry"
    );
    let ActionState::Failed(message) = world.state(cx, ActionKind::CreateGroup) else {
        panic!("the action failed")
    };
    assert!(!message.is_empty());
}

// ---- Editing ----------------------------------------------------------------------------------------

#[gpui::test]
fn editing_sends_only_what_changed_and_an_unchanged_save_sends_nothing(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.admin(cx);
    world.open_dialog(cx);
    cx.simulate_keystrokes("down enter");

    // Saving without a change closes the editor without a request.
    world.click(cx, "group-save");
    assert!(world.on_screen(cx, "groups-list"), "back at the list");
    assert!(
        world.take_effects().is_empty(),
        "nothing was asked of the server"
    );

    // Rename: handle and people are left alone.
    cx.simulate_keystrokes("enter");
    cx.simulate_keystrokes("tab ctrl-a backspace");
    cx.simulate_input("Designers");
    cx.simulate_keystrokes("enter");
    let update = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::UpdateGroup { group, request } => Some((group, request)),
            _ => None,
        })
        .expect("saving asks the server to update");
    assert_eq!(update.0, world.team.id);
    assert_eq!(update.1.handle, None);
    assert_eq!(
        update.1.name.as_ref().map(|n| n.as_str()),
        Some("Designers")
    );
    assert_eq!(update.1.members, None);

    let mut renamed = world.team.clone();
    renamed.name = DisplayName::parse("Designers").unwrap();
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::UpdateGroup(world.team.id),
            result: Ok(ActionOutcome::Group(renamed)),
        },
    );
    assert!(world.on_screen(cx, "groups-list"));
    assert!(
        world
            .groups(cx)
            .iter()
            .any(|(handle, name, _)| handle == "team" && name == "Designers")
    );
}

#[gpui::test]
fn people_can_be_removed_from_a_group_and_the_new_membership_is_sent_whole(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.admin(cx);
    world.open_dialog(cx);
    cx.simulate_keystrokes("down enter");
    // Chips for alice and bob show; remove bob (the last chip) with Backspace in the empty box.
    cx.simulate_keystrokes("tab tab backspace");
    cx.simulate_keystrokes("enter");
    let update = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::UpdateGroup { request, .. } => Some(request),
            _ => None,
        })
        .expect("saving asks the server to update");
    assert_eq!(update.members, Some(vec![world.me.id]), "the complete list");
    assert_eq!(update.name, None);
}

// ---- Deleting ---------------------------------------------------------------------------------------

#[gpui::test]
fn deleting_asks_first_and_cancelling_changes_nothing(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.admin(cx);
    world.open_dialog(cx);
    cx.simulate_keystrokes("delete");
    assert!(
        world.on_screen(cx, "group-delete-confirm"),
        "a confirmation first"
    );
    assert!(world.take_effects().is_empty());

    // Escape backs out to the list, not out of the dialog.
    cx.simulate_keystrokes("escape");
    assert!(world.on_screen(cx, "groups-list"));
    assert!(world.take_effects().is_empty());
    assert_eq!(world.groups(cx).len(), 2);

    // The confirmation starts on Cancel, so Enter alone never deletes.
    cx.simulate_keystrokes("delete enter");
    assert!(world.on_screen(cx, "groups-list"));
    assert!(
        world.take_effects().is_empty(),
        "Enter on Cancel deletes nothing"
    );
}

#[gpui::test]
fn confirming_deletes_the_chosen_group(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.admin(cx);
    world.open_dialog(cx);
    cx.simulate_keystrokes("down delete");
    world.click(cx, "group-delete-confirm");
    let deleted: Vec<GroupId> = world
        .take_effects()
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::DeleteGroup { group } => Some(group),
            _ => None,
        })
        .collect();
    assert_eq!(deleted, vec![world.team.id], "the highlighted group, once");

    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::DeleteGroup(world.team.id),
            result: Ok(ActionOutcome::Done),
        },
    );
    assert!(world.on_screen(cx, "groups-list"), "back at the list");
    assert_eq!(
        world.groups(cx),
        vec![("ops".to_owned(), "Operations".to_owned(), 1)]
    );
}

#[gpui::test]
fn a_failed_delete_is_explained_and_can_be_retried(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.admin(cx);
    world.open_dialog(cx);
    cx.simulate_keystrokes("delete");
    world.click(cx, "group-delete-confirm");
    world.take_effects();
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::DeleteGroup(world.ops.id),
            result: Err(RequestError::Unknown("timed out".into())),
        },
    );
    assert!(world.on_screen(cx, "group-error"), "the failure is shown");
    assert!(
        world.on_screen(cx, "group-delete-confirm"),
        "and it can be tried again"
    );
    world.click(cx, "group-delete-confirm");
    assert!(
        world.take_effects().iter().any(
            |effect| matches!(effect, Effect::DeleteGroup { group } if *group == world.ops.id)
        ),
        "the retry asks again"
    );
}

// ---- Focus and the keyboard ---------------------------------------------------------------------------

#[gpui::test]
fn escape_steps_back_one_view_at_a_time_and_focus_returns_to_the_entry(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.admin(cx);
    let entry = world.groups_focus(cx);
    // Open with the keyboard on a sidebar control: the dialog takes it and gives it back.
    cx.update(|window, cx| window.focus(&entry, cx));
    world.open_in_place(cx);
    assert!(
        !cx.update(|window, _| entry.is_focused(window)),
        "focus moved into the dialog"
    );

    cx.simulate_keystrokes("enter");
    assert!(world.on_screen(cx, "group-save"));
    cx.simulate_keystrokes("escape");
    assert!(
        world.on_screen(cx, "groups-list"),
        "Escape leaves the editor first"
    );
    assert!(world.on_screen(cx, "dialog-card"));

    cx.simulate_keystrokes("escape");
    assert!(
        !world.on_screen(cx, "dialog-card"),
        "Escape closes the dialog"
    );
    assert!(
        cx.update(|window, _| entry.is_focused(window)),
        "focus is back on the entry that opened it"
    );
}

#[gpui::test]
fn tab_stays_inside_the_dialog_in_every_view(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.admin(cx);
    let entry = world.groups_focus(cx);
    world.open_dialog(cx);
    let escapes = |cx: &mut VisualTestContext| cx.update(|window, _| entry.is_focused(window));

    for _ in 0..12 {
        cx.simulate_keystrokes("tab");
        assert!(!escapes(cx), "Tab never leaves the list view");
    }
    for _ in 0..12 {
        cx.simulate_keystrokes("shift-tab");
        assert!(!escapes(cx), "Shift+Tab never leaves the list view");
    }
    world.click(cx, "groups-new");
    for _ in 0..12 {
        cx.simulate_keystrokes("tab");
        assert!(!escapes(cx), "Tab never leaves the editor");
    }
    for _ in 0..12 {
        cx.simulate_keystrokes("shift-tab");
        assert!(!escapes(cx), "Shift+Tab never leaves the editor");
    }
}

#[gpui::test]
fn the_list_follows_groups_that_change_while_it_is_open(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.admin(cx);
    world.open_dialog(cx);
    let added = UserGroup {
        id: GroupId::generate(),
        workspace_id: world.workspace,
        handle: GroupHandle::parse("design").unwrap(),
        name: DisplayName::parse("Design").unwrap(),
        members: vec![],
    };
    world.event(cx, 1, Event::GroupChanged { group: added });
    assert!(
        world.on_screen(cx, "group-row-2"),
        "a group created by another admin appears"
    );
}

#[gpui::test]
fn a_group_deleted_by_someone_else_while_editing_ends_the_edit_gracefully(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.admin(cx);
    world.open_dialog(cx);
    cx.simulate_keystrokes("down enter");
    world.event(
        cx,
        1,
        Event::GroupDeleted {
            group_id: world.team.id,
        },
    );
    world.take_effects();
    cx.simulate_keystrokes("tab ctrl-a backspace");
    cx.simulate_input("Late edit");
    cx.simulate_keystrokes("enter");
    assert!(
        world.take_effects().is_empty(),
        "nothing is sent for a group that is gone"
    );
    assert!(
        world.on_screen(cx, "groups-list"),
        "the person is returned to the list"
    );
}
