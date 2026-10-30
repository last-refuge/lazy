//! Slice 3F on GPUI's test platform: the administration commands (the Workspace menu, and who may
//! run them), the sidebar's finders and footer, the workspace settings
//! dialog, the invitations dialog and the one-time code, roles and deactivation in the profile,
//! the people directory's deactivated rows, the register form's invitation code, the no-access
//! screen, and the new-channel affordance under the channel policy. The real views and the real
//! core run; a recording effect sink stands in for the runtime.
//!
//! Every keyboard claim is made with real key events through GPUI's dispatch, every mouse claim
//! with real mouse events. The test platform gives every character the same width and never
//! reports a window as active; whether a screen looks right is decided by the offscreen renders
//! (`.dev/screens-3f`). Nothing here exercises a real input method or screen reader.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, Modifiers, TestAppContext, VisualTestContext, WindowAppearance,
};
use lazy_client_core::{ActionKind, ActionOutcome, Effect, Input, RequestError, StartupState};
use lazy_domain::notification::{ConversationNotificationPrefs, NotificationSettings};
use lazy_domain::workspace::{ChannelCreationPolicy, RegistrationPolicy, WorkspaceSettings};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    MessageDocument, MessageId, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::InviteCode;
use lazy_protocol::{
    BootstrapResponse, Conversation, ConversationUserState, HistoryPage, Message, PROTOCOL_VERSION,
    User, Workspace, WorkspaceBootstrap,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::commands::CommandId;
use lazy_ui::root_view::RootView;
use lazy_ui::workspace_form::{Expiry, Uses};

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
    carol: User,
    dm: ConversationId,
    old: ConversationId,
    /// Messages the bootstrap says are saved.
    saved: RefCell<Vec<MessageId>>,
    /// Whether the person is in an archived channel and has muted #design.
    extras: bool,
    /// Their role in the workspace, and the workspace's settings.
    role: std::cell::Cell<WorkspaceRole>,
    settings: std::cell::Cell<WorkspaceSettings>,
    /// Members whose membership is deactivated.
    deactivated: RefCell<Vec<UserId>>,
    /// The generation of the request for who is in #general, as the last `reach` saw it.
    members_generation: std::cell::Cell<Option<u64>>,
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
        alice: person("alice", "Alice Nguyen"),
        bob: person("bob", "Bob Okafor"),
        workspace: WorkspaceId::generate(),
        general: ConversationId::generate(),
        design: ConversationId::generate(),
        carol: person("carol", "Carol Mbeki"),
        dm: ConversationId::generate(),
        old: ConversationId::generate(),
        saved: RefCell::new(Vec::new()),
        extras: false,
        role: std::cell::Cell::new(WorkspaceRole::Member),
        settings: std::cell::Cell::new(WorkspaceSettings::default()),
        deactivated: RefCell::new(Vec::new()),
        members_generation: std::cell::Cell::new(None),
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

    fn bootstrap(&self, role: WorkspaceRole) -> BootstrapResponse {
        let state = |id: ConversationId, unread: u32, mentions: u32| ConversationUserState {
            conversation_id: id,
            favorite: false,
            last_read: EventSeq::ZERO,
            unread_count: unread,
            mention_count: mentions,
        };
        let mut conversations = vec![
            self.channel(self.general, "general", true),
            self.channel(self.design, "design", false),
            Conversation {
                kind: ConversationKind::Direct,
                name: None,
                participants: vec![self.me.id, self.bob.id],
                ..self.channel(self.dm, "placeholder", false)
            },
        ];
        let mut user_state = vec![
            state(self.general, 0, 0),
            state(self.design, 5, 1),
            state(self.dm, 2, 0),
        ];
        let mut prefs = Vec::new();
        if self.extras {
            let mut old = self.channel(self.old, "old-launch", false);
            old.archived = true;
            conversations.push(old);
            user_state.push(state(self.old, 0, 0));
            prefs.push(ConversationNotificationPrefs {
                conversation_id: self.design,
                level: None,
                muted: true,
            });
        }
        BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: self.me.clone(),
            workspaces: vec![WorkspaceBootstrap {
                saved: self.saved.borrow().clone(),
                notification_settings: NotificationSettings::default(),
                conversation_notification_prefs: prefs,
                custom_emoji: Vec::new(),
                threads: Vec::new(),
                groups: Vec::new(),
                workspace: Workspace {
                    id: self.workspace,
                    name: "Lazy".into(),
                    settings: self.settings.get(),
                },
                role,
                conversations,
                user_state,
                members: [&self.me, &self.alice, &self.bob, &self.carol]
                    .into_iter()
                    .map(|user| User {
                        deactivated: self.deactivated.borrow().contains(&user.id),
                        ..user.clone()
                    })
                    .collect(),
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

    fn reach(&self, cx: &mut VisualTestContext) {
        // A big window, so the dialogs and the inbox beside the pane have the room they need.
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(1600.)));
        self.dispatch(
            cx,
            Input::Started(StartupState {
                session_restored: true,
                ..Default::default()
            }),
        );
        self.dispatch(
            cx,
            Input::BootstrapLoaded(Box::new(self.bootstrap(self.role.get()))),
        );
        // The runtime sends the clock; here the test does.
        self.dispatch(cx, Input::Clock(Timestamp::now()));
        let effects = self.take_effects();
        self.members_generation
            .set(effects.iter().find_map(|effect| match effect {
                Effect::FetchMembers { generation, .. } => Some(*generation),
                _ => None,
            }));
        let generation = effects
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
                    messages: vec![self.message(self.general, &self.alice, "welcome", 1)],
                    has_more: false,
                }),
            },
        );
        draw(cx);
    }

    fn message(
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
}

impl World {
    fn core<R>(
        &self,
        cx: &VisualTestContext,
        read: impl FnOnce(&lazy_client_core::ClientCore) -> R,
    ) -> R {
        self.model.read_with(cx, |model, _| read(model.core()))
    }

    /// Effects of one kind, oldest first, taken out of the recorder.
    fn drain<T>(&self, pick: impl Fn(&Effect) -> Option<T>) -> Vec<T> {
        let mut effects = self.effects.0.borrow_mut();
        let mut picked = Vec::new();
        effects.retain(|effect| match pick(effect) {
            Some(item) => {
                picked.push(item);
                false
            }
            None => true,
        });
        picked
    }
}

// ----- Helpers -----

impl World {
    /// Starts signed in with `role`.
    fn reach_as(&self, cx: &mut VisualTestContext, role: WorkspaceRole) {
        self.role.set(role);
        self.reach(cx);
    }

    /// Delivers a realtime event of the workspace.
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
        draw(cx);
    }

    /// Whether the registry lets the person run `id` now (the rule every button, menu item, and
    /// shortcut asks).
    fn can_run(&self, cx: &VisualTestContext, id: CommandId) -> bool {
        let snapshot = self.core(cx, lazy_ui::commands::AppSnapshot::from_core);
        lazy_ui::commands::availability(id, &snapshot).is_enabled()
    }

    fn open_dialog(&self, cx: &mut VisualTestContext, dialog: lazy_ui::shell::ShellDialog) {
        let shell = self.shell(cx);
        cx.update(|window, app| {
            shell.update(app, |shell, cx| shell.open_dialog(dialog, window, cx))
        });
        draw(cx);
    }

    fn settings_dialog(
        &self,
        cx: &mut VisualTestContext,
    ) -> Entity<lazy_ui::dialogs::WorkspaceSettingsDialog> {
        let shell = self.shell(cx);
        shell
            .read_with(cx, |shell, _| shell.workspace_settings_dialog())
            .expect("the settings are open")
    }

    fn settings_stop(&self, cx: &mut VisualTestContext) -> &'static str {
        let dialog = self.settings_dialog(cx);
        cx.update(|window, app| dialog.read(app).stop(window, app))
    }

    fn invitations_dialog(
        &self,
        cx: &mut VisualTestContext,
    ) -> Entity<lazy_ui::dialogs::InvitationsDialog> {
        let shell = self.shell(cx);
        shell
            .read_with(cx, |shell, _| shell.invitations_dialog())
            .expect("the invitations are open")
    }

    fn invitations_stop(&self, cx: &mut VisualTestContext) -> &'static str {
        let dialog = self.invitations_dialog(cx);
        cx.update(|window, app| dialog.read(app).stop(window, app))
    }

    fn profile_stop(&self, cx: &mut VisualTestContext) -> &'static str {
        let shell = self.shell(cx);
        let dialog = shell
            .read_with(cx, |shell, _| shell.profile_dialog())
            .expect("a profile is open");
        cx.update(|window, app| dialog.read(app).stop(window))
    }

    fn open_profile(&self, cx: &mut VisualTestContext, user: UserId) {
        let shell = self.shell(cx);
        cx.update(|window, app| {
            shell.update(app, |shell, cx| shell.open_profile(user, window, cx))
        });
        draw(cx);
    }

    fn person_entry(&self, user: &User, role: WorkspaceRole) -> lazy_protocol::Person {
        lazy_protocol::Person {
            user: user.clone(),
            role,
            joined_at: Timestamp::from(time::OffsetDateTime::now_utc() - time::Duration::days(400)),
        }
    }

    fn answer_profile(&self, cx: &mut VisualTestContext, person: lazy_protocol::Person) {
        let generation = self
            .drain(|effect| match effect {
                Effect::LoadProfile { generation, .. } => Some(*generation),
                _ => None,
            })
            .pop()
            .expect("the profile was asked for");
        self.dispatch(
            cx,
            Input::ProfileLoaded {
                generation,
                result: Ok(lazy_protocol::PersonResponse { person }),
            },
        );
        draw(cx);
    }

    fn answer_invitations(
        &self,
        cx: &mut VisualTestContext,
        invitations: Vec<lazy_protocol::Invitation>,
    ) {
        let generation = self
            .drain(|effect| match effect {
                Effect::LoadInvitations { generation, .. } => Some(*generation),
                _ => None,
            })
            .pop()
            .expect("the invitations were asked for");
        self.dispatch(
            cx,
            Input::InvitationsLoaded {
                generation,
                result: Ok(lazy_protocol::InvitationList { invitations }),
            },
        );
        draw(cx);
    }

    fn invitation(
        &self,
        state: lazy_protocol::InvitationState,
        uses: u32,
        max: Option<u32>,
        expires_in_minutes: i64,
    ) -> lazy_protocol::Invitation {
        lazy_protocol::Invitation {
            id: lazy_domain::InvitationId::generate(),
            workspace_id: self.workspace,
            created_by: self.alice.id,
            created_at: Timestamp::from(time::OffsetDateTime::now_utc() - time::Duration::hours(3)),
            expires_at: Timestamp::from(
                time::OffsetDateTime::now_utc() + time::Duration::minutes(expires_in_minutes),
            ),
            max_uses: max,
            uses,
            state,
        }
    }

    fn rejected(code: ErrorCode, message: &str) -> RequestError {
        RequestError::Rejected(ApiError::new(code, message))
    }
}

use lazy_protocol::{ApiError, ErrorCode, Event, EventEnvelope, ServerFrame};

// ----- The sidebar and the Workspace menu -----

#[gpui::test]
fn admins_and_owners_can_run_the_administration_commands_and_members_cannot(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Member);
    assert!(!world.can_run(cx, CommandId::WorkspaceSettings));
    assert!(!world.can_run(cx, CommandId::InvitePeople));
    assert!(
        world.on_screen(cx, "sidebar-preferences"),
        "Preferences is everyone's: the person's own row opens it"
    );
    // Being made an admin allows them at once, being demoted takes them away at once.
    world.event(
        cx,
        10,
        Event::MemberRoleChanged {
            user_id: world.me.id,
            role: WorkspaceRole::Admin,
        },
    );
    assert!(world.can_run(cx, CommandId::WorkspaceSettings));
    assert!(world.can_run(cx, CommandId::InvitePeople));
    world.event(
        cx,
        11,
        Event::MemberRoleChanged {
            user_id: world.me.id,
            role: WorkspaceRole::Member,
        },
    );
    assert!(!world.can_run(cx, CommandId::WorkspaceSettings));
    assert!(!world.can_run(cx, CommandId::InvitePeople));
}

#[gpui::test]
fn the_workspace_menu_opens_the_dialogs_and_escape_returns_the_keyboard(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Owner);
    world.click(cx, "sidebar-menu");
    world.click(cx, "command-menu-tab-Workspace");
    world.click(cx, "command-menu-WorkspaceSettings");
    assert!(world.on_screen(cx, "settings-save"));
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "settings-save"));
    world.click(cx, "sidebar-menu");
    world.click(cx, "command-menu-tab-Workspace");
    world.click(cx, "command-menu-InvitePeople");
    assert!(world.on_screen(cx, "invite-create"));
    assert!(world.on_screen(cx, "invite-loading"), "the list is loading");
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "invite-create"));
    assert!(
        !world.core(cx, |core| core.invitations().open),
        "closing the dialog closes the list"
    );
}

#[gpui::test]
fn the_finders_share_a_row_and_the_footer_is_only_the_person_and_sign_out(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Owner);
    for (width, height) in [(640.0_f32, 480.0_f32), (1500.0, 1600.0)] {
        cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(height)));
        draw(cx);
        // The finders sit side by side, whatever the width, with no key hints on them.
        let search = cx.debug_bounds("sidebar-search").unwrap();
        let switcher = cx.debug_bounds("sidebar-switcher").unwrap();
        assert_eq!(search.top(), switcher.top(), "side by side at {width}");
        assert!(search.right() <= switcher.left(), "one beside the other");
        // The footer is the person's own row (which opens Preferences) and Sign out, on one
        // line, inside the window; the administration links are in the Menu only.
        let person = cx
            .debug_bounds("sidebar-preferences")
            .expect("the person's row");
        let sign_out = cx.debug_bounds("sign-out").expect("Sign out");
        assert!(f32::from(person.bottom()) <= height && f32::from(sign_out.bottom()) <= height);
        let apart = f32::from(person.center().y) - f32::from(sign_out.center().y);
        assert!(apart.abs() < 1.0, "one line for both at {width}");
        for selector in [
            "sidebar-workspace-settings",
            "sidebar-invite-people",
            "sidebar-user-groups",
            "sidebar-custom-emoji",
            "sidebar-admin",
        ] {
            assert!(
                !world.on_screen(cx, selector),
                "{selector} lives in the Menu, not the sidebar, at {width}"
            );
        }
    }
}

// ----- Workspace settings -----

#[gpui::test]
fn settings_open_with_the_name_and_save_waits_for_a_change(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Admin);
    world.open_dialog(cx, lazy_ui::shell::ShellDialog::WorkspaceSettings);
    let dialog = world.settings_dialog(cx);
    assert_eq!(dialog.read_with(cx, |d, app| d.name_text(app)), "Lazy");
    assert_eq!(world.settings_stop(cx), "name", "the name has the keyboard");
    assert!(
        !dialog.read_with(cx, |d, app| d.can_save(app)),
        "nothing changed, nothing to save"
    );
    world.take_effects();
    cx.simulate_keystrokes("enter");
    assert!(
        world
            .drain(|e| matches!(e, Effect::UpdateWorkspace { .. }).then_some(()))
            .is_empty(),
        "Enter in an unchanged name saves nothing"
    );
    cx.simulate_input(" Labs");
    assert!(dialog.read_with(cx, |d, app| d.can_save(app)));
    cx.simulate_keystrokes("enter");
    let sent = world.drain(|effect| match effect {
        Effect::UpdateWorkspace { request, .. } => Some(request.clone()),
        _ => None,
    });
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].name.as_ref().map(|n| n.as_str()), Some("Lazy Labs"));
    assert_eq!(sent[0].settings, None, "only what changed is sent");
}

#[gpui::test]
fn the_policies_are_chosen_from_the_keyboard_and_saved_together(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Owner);
    world.open_dialog(cx, lazy_ui::shell::ShellDialog::WorkspaceSettings);
    // Tab order: name, who may register, who may create channels, Save.
    cx.simulate_keystrokes("tab");
    assert_eq!(world.settings_stop(cx), "registration");
    cx.simulate_keystrokes("right");
    cx.simulate_keystrokes("tab");
    assert_eq!(world.settings_stop(cx), "channels");
    cx.simulate_keystrokes("right");
    let dialog = world.settings_dialog(cx);
    assert_eq!(
        dialog.read_with(cx, |d, _| d.choices()),
        (
            RegistrationPolicy::InviteOnly,
            ChannelCreationPolicy::AdminsOnly
        )
    );
    cx.simulate_keystrokes("tab");
    assert_eq!(world.settings_stop(cx), "save");
    world.take_effects();
    cx.simulate_keystrokes("enter");
    let sent = world.drain(|effect| match effect {
        Effect::UpdateWorkspace { request, .. } => Some(request.clone()),
        _ => None,
    });
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].name, None, "the name did not change");
    assert_eq!(
        sent[0].settings,
        Some(WorkspaceSettings {
            registration: RegistrationPolicy::InviteOnly,
            channel_creation: ChannelCreationPolicy::AdminsOnly,
        })
    );
}

#[gpui::test]
fn a_bad_name_says_what_to_change_and_a_refusal_is_shown_in_place(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Owner);
    world.open_dialog(cx, lazy_ui::shell::ShellDialog::WorkspaceSettings);
    cx.simulate_keystrokes("ctrl-a backspace");
    assert!(
        world.on_screen(cx, "field-error-Name"),
        "an empty name says so"
    );
    let dialog = world.settings_dialog(cx);
    assert!(!dialog.read_with(cx, |d, app| d.can_save(app)));
    cx.simulate_input("A better name");
    assert!(!world.on_screen(cx, "field-error-Name"));
    world.take_effects();
    cx.simulate_keystrokes("enter");
    assert!(world.on_screen(cx, "settings-save"));
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::UpdateWorkspace,
            result: Err(World::rejected(
                ErrorCode::Forbidden,
                "Only admins and owners can change the workspace.",
            )),
        },
    );
    assert!(
        world.on_screen(cx, "settings-failed"),
        "the server's refusal is shown"
    );
}

#[gpui::test]
fn the_dialog_follows_a_change_made_elsewhere_and_closes_when_the_person_is_demoted(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Admin);
    world.open_dialog(cx, lazy_ui::shell::ShellDialog::WorkspaceSettings);
    let dialog = world.settings_dialog(cx);
    // Another admin renames the workspace while this one has typed nothing: the field follows.
    world.event(
        cx,
        20,
        Event::WorkspaceUpdated {
            workspace: Workspace {
                id: world.workspace,
                name: "Lazy Labs".into(),
                settings: WorkspaceSettings {
                    registration: RegistrationPolicy::InviteOnly,
                    ..Default::default()
                },
            },
        },
    );
    assert_eq!(dialog.read_with(cx, |d, app| d.name_text(app)), "Lazy Labs");
    assert_eq!(
        dialog.read_with(cx, |d, _| d.choices().0),
        RegistrationPolicy::InviteOnly
    );
    assert!(
        !dialog.read_with(cx, |d, app| d.can_save(app)),
        "what it shows is what is saved"
    );
    // Demoted: the dialog has nothing to offer and closes.
    world.event(
        cx,
        21,
        Event::MemberRoleChanged {
            user_id: world.me.id,
            role: WorkspaceRole::Member,
        },
    );
    assert!(!world.on_screen(cx, "settings-save"));
}

// ----- Invitations -----

#[gpui::test]
fn the_list_shows_where_each_invitation_stands_and_only_active_ones_can_be_revoked(
    cx: &mut TestAppContext,
) {
    use lazy_protocol::InvitationState::{Active, Expired, Revoked, UsedUp};
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Owner);
    world.open_dialog(cx, lazy_ui::shell::ShellDialog::Invitations);
    assert!(world.on_screen(cx, "invite-loading"));
    world.answer_invitations(
        cx,
        vec![
            world.invitation(Active, 2, Some(5), 60 * 24 * 3),
            world.invitation(UsedUp, 1, Some(1), 60 * 24),
            world.invitation(Revoked, 0, None, 60 * 24),
            world.invitation(Expired, 3, Some(25), -60 * 24 * 2),
        ],
    );
    assert!(!world.on_screen(cx, "invite-loading"));
    for index in 0..4 {
        let selector: &'static str = Box::leak(format!("invite-row-{index}").into_boxed_str());
        assert!(world.on_screen(cx, selector), "{selector}");
    }
    assert!(
        world.on_screen(cx, "invite-revoke-0"),
        "an active invitation can be revoked"
    );
    for index in 1..4 {
        let selector: &'static str = Box::leak(format!("invite-revoke-{index}").into_boxed_str());
        assert!(!world.on_screen(cx, selector), "{selector} is not offered");
    }
}

#[gpui::test]
fn an_invitation_is_made_with_the_chosen_expiry_and_uses_and_its_code_shows_once(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Owner);
    world.open_dialog(cx, lazy_ui::shell::ShellDialog::Invitations);
    world.answer_invitations(cx, Vec::new());
    assert!(world.on_screen(cx, "invite-empty"));
    // The keyboard is in the form: expiry (Left and Right choose), uses, then Create.
    assert_eq!(world.invitations_stop(cx), "expiry");
    cx.simulate_keystrokes("left");
    cx.simulate_keystrokes("tab");
    assert_eq!(world.invitations_stop(cx), "uses");
    cx.simulate_keystrokes("right right");
    cx.simulate_keystrokes("tab");
    assert_eq!(world.invitations_stop(cx), "create");
    let dialog = world.invitations_dialog(cx);
    assert_eq!(
        dialog.read_with(cx, |d, _| d.choices()),
        (Expiry::Day, Uses::Five)
    );
    world.take_effects();
    cx.simulate_keystrokes("enter");
    let sent = world.drain(|effect| match effect {
        Effect::CreateInvitation { request, .. } => Some(request.clone()),
        _ => None,
    });
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].expires_in_secs, Some(24 * 60 * 60));
    assert_eq!(sent[0].max_uses, Some(5));
    // The answer carries the code, shown in a field of its own with the warning.
    let created = world.invitation(lazy_protocol::InvitationState::Active, 0, Some(5), 60 * 24);
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::CreateInvitation,
            result: Ok(ActionOutcome::InvitationCreated(
                lazy_protocol::CreatedInvitation {
                    invitation: created,
                    code: InviteCode::new(CODE),
                },
            )),
        },
    );
    draw(cx);
    assert!(world.on_screen(cx, "invite-created"));
    assert!(world.on_screen(cx, "invite-shown-once"));
    assert_eq!(
        world.invitations_stop(cx),
        "code",
        "the code has the keyboard"
    );
    // It can be copied...
    cx.simulate_keystrokes("tab");
    assert_eq!(world.invitations_stop(cx), "copy");
    cx.simulate_keystrokes("enter");
    assert_eq!(
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .as_deref(),
        Some(CODE)
    );
    // ...and cannot be edited: typing in its field changes nothing.
    cx.simulate_keystrokes("shift-tab");
    cx.simulate_input("zzz");
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_keystrokes("ctrl-c");
    assert_eq!(
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .as_deref(),
        Some(CODE),
        "the field still holds the code"
    );
    // The whole message to send can be copied too: where to get Lazy, the server, and the code.
    cx.simulate_keystrokes("tab tab");
    assert_eq!(world.invitations_stop(cx), "copy-message");
    cx.simulate_keystrokes("enter");
    let message = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("the message is on the clipboard");
    for wanted in [
        "Join us on Lazy.",
        "1. Get Lazy: ",
        "2. Server: ",
        CODE,
        " until ",
    ] {
        assert!(message.contains(wanted), "{wanted:?} is not in {message:?}");
    }
    // Done forgets it.
    cx.simulate_keystrokes("tab");
    assert_eq!(world.invitations_stop(cx), "done");
    cx.simulate_keystrokes("enter");
    assert!(!world.on_screen(cx, "invite-created"));
    assert!(
        world.core(cx, |core| core.invitations().created.is_none()),
        "the core let it go"
    );
}

const CODE: &str = "Qx7mR2vB9tLw3KpZ8cYd5N";

#[gpui::test]
fn closing_the_dialog_forgets_a_code_that_was_never_dismissed(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Owner);
    world.open_dialog(cx, lazy_ui::shell::ShellDialog::Invitations);
    world.answer_invitations(cx, Vec::new());
    cx.simulate_keystrokes("tab tab enter");
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::CreateInvitation,
            result: Ok(ActionOutcome::InvitationCreated(
                lazy_protocol::CreatedInvitation {
                    invitation: world.invitation(
                        lazy_protocol::InvitationState::Active,
                        0,
                        None,
                        60,
                    ),
                    code: InviteCode::new(CODE),
                },
            )),
        },
    );
    assert!(world.on_screen(cx, "invite-created"));
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "invite-created"));
    assert!(world.core(cx, |core| core.invitations().created.is_none()));
    // Opening again shows no code.
    world.open_dialog(cx, lazy_ui::shell::ShellDialog::Invitations);
    assert!(
        !world.on_screen(cx, "invite-created"),
        "a lost code is gone for good"
    );
}

#[gpui::test]
fn revoke_asks_first_and_escape_keeps_the_invitation(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Owner);
    world.open_dialog(cx, lazy_ui::shell::ShellDialog::Invitations);
    let active = world.invitation(lazy_protocol::InvitationState::Active, 1, Some(5), 60 * 24);
    let id = active.id;
    world.answer_invitations(cx, vec![active.clone()]);
    world.click(cx, "invite-revoke-0");
    assert!(world.on_screen(cx, "invite-confirm-0"), "it asks first");
    assert_eq!(
        world.invitations_stop(cx),
        "keep",
        "the safe answer has the keyboard"
    );
    world.take_effects();
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "invite-confirm-0"), "Escape keeps it");
    assert!(world.on_screen(cx, "invite-create"), "and the dialog stays");
    assert!(
        world
            .drain(|e| matches!(e, Effect::RevokeInvitation { .. }).then_some(()))
            .is_empty()
    );
    // Yes revokes it.
    world.click(cx, "invite-revoke-0");
    world.click(cx, "invite-revoke-yes");
    let revoked = world.drain(|effect| match effect {
        Effect::RevokeInvitation { invitation } => Some(*invitation),
        _ => None,
    });
    assert_eq!(revoked, vec![id]);
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::RevokeInvitation(id),
            result: Ok(ActionOutcome::Invitation(lazy_protocol::Invitation {
                state: lazy_protocol::InvitationState::Revoked,
                ..active
            })),
        },
    );
    draw(cx);
    assert!(
        !world.on_screen(cx, "invite-revoke-0"),
        "a revoked invitation offers nothing more"
    );
}

#[gpui::test]
fn the_list_says_when_it_failed_and_keeps_the_keyboard_after_try_again(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Owner);
    world.open_dialog(cx, lazy_ui::shell::ShellDialog::Invitations);
    let generation = world
        .drain(|effect| match effect {
            Effect::LoadInvitations { generation, .. } => Some(*generation),
            _ => None,
        })
        .pop()
        .unwrap();
    world.dispatch(
        cx,
        Input::InvitationsLoaded {
            generation,
            result: Err(RequestError::Unknown("connection reset".into())),
        },
    );
    draw(cx);
    assert!(world.on_screen(cx, "invite-failed"));
    world.click(cx, "invite-retry");
    assert!(
        !world
            .drain(|e| matches!(e, Effect::LoadInvitations { .. }).then_some(()))
            .is_empty(),
        "Try again asks again"
    );
    cx.simulate_keystrokes("escape");
    assert!(
        !world.on_screen(cx, "invite-create"),
        "and Escape still closes the dialog"
    );
}

#[test]
fn the_invitation_code_is_exposed_in_one_place_only() {
    // A code may be put in exactly one element: the selectable field of the invitations dialog.
    // Anything else that formats or exposes one (a tooltip, a title, a label, a log line) would
    // leak it, so this reads the sources and counts.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                let mentions = text.matches("code.expose()").count();
                if mentions > 0 {
                    found.push((
                        path.file_name().unwrap().to_string_lossy().into_owned(),
                        mentions,
                    ));
                }
            }
        }
    }
    assert_eq!(
        found,
        vec![("invitations.rs".to_owned(), 1)],
        "an invitation code is read out of the core in one place"
    );
}

// ----- Roles and deactivation in the profile -----

#[gpui::test]
fn an_owner_may_give_any_role_and_the_choice_is_sent(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Owner);
    world.open_profile(cx, world.bob.id);
    world.answer_profile(cx, world.person_entry(&world.bob, WorkspaceRole::Member));
    assert!(world.on_screen(cx, "profile-admin"));
    for selector in [
        "profile-set-owner",
        "profile-set-admin",
        "profile-set-member",
    ] {
        assert!(
            world.on_screen(cx, selector),
            "{selector} is offered to an owner"
        );
    }
    world.take_effects();
    world.click(cx, "profile-set-admin");
    let sent = world.drain(|effect| match effect {
        Effect::SetMemberRole { user, request, .. } => Some((*user, request.role)),
        _ => None,
    });
    assert_eq!(sent, vec![(world.bob.id, WorkspaceRole::Admin)]);
}

#[gpui::test]
fn an_admin_is_offered_only_member_and_admin_and_nothing_for_an_owner(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Admin);
    world.open_profile(cx, world.bob.id);
    world.answer_profile(cx, world.person_entry(&world.bob, WorkspaceRole::Member));
    assert!(world.on_screen(cx, "profile-set-admin") && world.on_screen(cx, "profile-set-member"));
    assert!(
        !world.on_screen(cx, "profile-set-owner"),
        "admins never make owners"
    );
    assert!(
        world.on_screen(cx, "profile-deactivate"),
        "an admin may deactivate a member"
    );
    cx.simulate_keystrokes("escape");
    // An owner's profile has nothing to administer for an admin.
    world.open_profile(cx, world.alice.id);
    world.answer_profile(cx, world.person_entry(&world.alice, WorkspaceRole::Owner));
    assert!(
        !world.on_screen(cx, "profile-admin"),
        "admins never touch an owner"
    );
    cx.simulate_keystrokes("escape");
    // And another admin can be moved but not deactivated.
    world.open_profile(cx, world.carol.id);
    world.answer_profile(cx, world.person_entry(&world.carol, WorkspaceRole::Admin));
    assert!(world.on_screen(cx, "profile-role"));
    assert!(!world.on_screen(cx, "profile-deactivate"));
}

#[gpui::test]
fn a_member_sees_no_administration_and_nobody_deactivates_themself(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Member);
    world.open_profile(cx, world.bob.id);
    world.answer_profile(cx, world.person_entry(&world.bob, WorkspaceRole::Member));
    assert!(!world.on_screen(cx, "profile-admin"));
    cx.simulate_keystrokes("escape");
    // An owner looking at themself may change their role (the server keeps an owner) but never
    // offers to deactivate.
    let (world, cx) = (world, cx);
    world.event(
        cx,
        30,
        Event::MemberRoleChanged {
            user_id: world.me.id,
            role: WorkspaceRole::Owner,
        },
    );
    world.open_profile(cx, world.me.id);
    world.answer_profile(cx, world.person_entry(&world.me, WorkspaceRole::Owner));
    assert!(world.on_screen(cx, "profile-role"));
    assert!(
        !world.on_screen(cx, "profile-deactivate"),
        "nobody deactivates themself"
    );
    // The server's refusal to demote the last owner is shown in place.
    world.click(cx, "profile-set-member");
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::SetMemberRole(world.me.id),
            result: Err(World::rejected(
                ErrorCode::Conflict,
                "a workspace needs at least one owner",
            )),
        },
    );
    assert!(world.on_screen(cx, "profile-role-failed"));
}

#[gpui::test]
fn deactivating_names_the_person_and_what_happens_and_escape_cancels(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Owner);
    world.open_profile(cx, world.carol.id);
    world.answer_profile(cx, world.person_entry(&world.carol, WorkspaceRole::Member));
    world.take_effects();
    world.click(cx, "profile-deactivate");
    assert!(world.on_screen(cx, "profile-confirm"), "it asks first");
    assert_eq!(
        world.profile_stop(cx),
        "cancel",
        "the safe answer has the keyboard"
    );
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "profile-confirm"), "Escape cancels");
    assert!(world.on_screen(cx, "profile-name"), "and the profile stays");
    assert_eq!(world.profile_stop(cx), "deactivate");
    assert!(
        world
            .drain(|e| matches!(e, Effect::SetMemberDeactivated { .. }).then_some(()))
            .is_empty()
    );
    // Yes sends it.
    world.click(cx, "profile-deactivate");
    world.click(cx, "profile-confirm-yes");
    let sent = world.drain(|effect| match effect {
        Effect::SetMemberDeactivated { user, request, .. } => Some((*user, request.deactivated)),
        _ => None,
    });
    assert_eq!(sent, vec![(world.carol.id, true)]);
    // A refusal is shown in place.
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::SetMemberDeactivated(world.carol.id),
            result: Err(World::rejected(
                ErrorCode::Forbidden,
                "you cannot do this to this person",
            )),
        },
    );
    assert!(world.on_screen(cx, "profile-activation-failed"));
}

#[gpui::test]
fn a_deactivated_person_is_marked_offers_reactivate_and_cannot_be_messaged(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.deactivated.borrow_mut().push(world.carol.id);
    world.reach_as(cx, WorkspaceRole::Owner);
    let mut person = world.person_entry(&world.carol, WorkspaceRole::Member);
    person.user.deactivated = true;
    world.open_profile(cx, world.carol.id);
    world.answer_profile(cx, person);
    assert!(
        world.on_screen(cx, "profile-deactivated"),
        "the badge says so"
    );
    assert!(world.on_screen(cx, "profile-deactivated-note"));
    assert!(world.on_screen(cx, "profile-reactivate"));
    assert!(!world.on_screen(cx, "profile-deactivate"));
    assert!(
        !world.on_screen(cx, "profile-message"),
        "no new conversation with them"
    );
    world.take_effects();
    world.click(cx, "profile-reactivate");
    world.click(cx, "profile-confirm-yes");
    let sent = world.drain(|effect| match effect {
        Effect::SetMemberDeactivated { user, request, .. } => Some((*user, request.deactivated)),
        _ => None,
    });
    assert_eq!(sent, vec![(world.carol.id, false)]);
}

// ----- The people directory -----

#[gpui::test]
fn admins_can_show_deactivated_people_and_those_rows_say_so(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Admin);
    world.click(cx, "sidebar-people");
    let everyone = vec![
        world.person_entry(&world.me, WorkspaceRole::Admin),
        world.person_entry(&world.bob, WorkspaceRole::Member),
    ];
    let generation = world
        .drain(|effect| match effect {
            Effect::LoadPeople { generation, .. } => Some(*generation),
            _ => None,
        })
        .pop()
        .unwrap();
    world.dispatch(
        cx,
        Input::PeopleLoaded {
            generation,
            result: Ok(lazy_protocol::PeoplePage {
                people: everyone,
                has_more: false,
            }),
        },
    );
    draw(cx);
    assert!(world.on_screen(cx, "people-show-deactivated"));
    assert!(!world.on_screen(cx, "people-deactivated-0"));
    world.take_effects();
    world.click(cx, "people-show-deactivated");
    let asked = world.drain(|effect| match effect {
        Effect::LoadPeople {
            generation, query, ..
        } => Some((*generation, query.include_deactivated)),
        _ => None,
    });
    let (generation, include) = asked.last().copied().expect("it asks again");
    assert_eq!(include, Some(true));
    let mut carol = world.person_entry(&world.carol, WorkspaceRole::Member);
    carol.user.deactivated = true;
    world.dispatch(
        cx,
        Input::PeopleLoaded {
            generation,
            result: Ok(lazy_protocol::PeoplePage {
                people: vec![world.person_entry(&world.bob, WorkspaceRole::Member), carol],
                has_more: false,
            }),
        },
    );
    draw(cx);
    assert!(
        world.on_screen(cx, "people-deactivated-1"),
        "the deactivated row says so"
    );
    assert!(!world.on_screen(cx, "people-deactivated-0"));
}

#[gpui::test]
fn members_do_not_get_the_toggle_and_a_role_badge_follows_a_change_live(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Member);
    world.click(cx, "sidebar-people");
    let generation = world
        .drain(|effect| match effect {
            Effect::LoadPeople { generation, .. } => Some(*generation),
            _ => None,
        })
        .pop()
        .unwrap();
    world.dispatch(
        cx,
        Input::PeopleLoaded {
            generation,
            result: Ok(lazy_protocol::PeoplePage {
                people: vec![
                    world.person_entry(&world.me, WorkspaceRole::Member),
                    world.person_entry(&world.bob, WorkspaceRole::Member),
                ],
                has_more: false,
            }),
        },
    );
    draw(cx);
    assert!(
        !world.on_screen(cx, "people-show-deactivated"),
        "the toggle is for admins"
    );
    assert!(!world.on_screen(cx, "people-role-1"));
    world.event(
        cx,
        40,
        Event::MemberRoleChanged {
            user_id: world.bob.id,
            role: WorkspaceRole::Admin,
        },
    );
    assert!(
        world.on_screen(cx, "people-role-1"),
        "the badge appears as the role changes"
    );
}

#[gpui::test]
fn the_details_member_list_marks_a_deactivated_person(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.deactivated.borrow_mut().push(world.bob.id);
    world.reach_as(cx, WorkspaceRole::Member);
    world.dispatch(cx, Input::SelectConversation(world.dm));
    world.take_effects();
    world.click(cx, "details-button");
    assert!(world.on_screen(cx, "details-member-bob"));
    assert!(world.on_screen(cx, "details-deactivated-bob"));
    assert!(!world.on_screen(cx, "details-deactivated-myself"));
}

// ----- Registering, and losing access -----

#[gpui::test]
fn the_register_form_has_an_optional_code_and_sends_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.dispatch(
        cx,
        Input::Started(StartupState {
            session_restored: false,
            ..Default::default()
        }),
    );
    draw(cx);
    // Signing in has no code field; creating an account has one, last before the button.
    assert!(!world.on_screen(cx, "sign-in-toggle-mode") || world.on_screen(cx, "sign-in-submit"));
    // To the toggle and press it, then back to the first field.
    cx.simulate_keystrokes("tab tab tab enter");
    cx.simulate_keystrokes("shift-tab shift-tab shift-tab shift-tab shift-tab");
    cx.simulate_input("newbie");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("Nora Newbie");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("correct horse");
    cx.simulate_keystrokes("tab");
    cx.simulate_input(CODE);
    world.take_effects();
    cx.simulate_keystrokes("enter");
    let sent = world.drain(|effect| match effect {
        Effect::Register(request) => Some(request.clone()),
        _ => None,
    });
    assert_eq!(sent.len(), 1, "the form registers");
    assert_eq!(
        sent[0].invite_code.as_ref().map(InviteCode::expose),
        Some(CODE),
        "with the code that was typed"
    );
}

#[gpui::test]
fn a_blank_code_registers_without_one_and_a_malformed_code_is_refused_before_a_request(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.dispatch(
        cx,
        Input::Started(StartupState {
            session_restored: false,
            ..Default::default()
        }),
    );
    draw(cx);
    // To the toggle and press it, then back to the first field.
    cx.simulate_keystrokes("tab tab tab enter");
    cx.simulate_keystrokes("shift-tab shift-tab shift-tab shift-tab shift-tab");
    cx.simulate_input("newbie");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("Nora Newbie");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("correct horse");
    world.take_effects();
    cx.simulate_keystrokes("enter");
    let sent = world.drain(|effect| match effect {
        Effect::Register(request) => Some(request.clone()),
        _ => None,
    });
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].invite_code, None, "a blank field sends no code");
    // A code that cannot be right never leaves the machine.
    world.dispatch(
        cx,
        Input::AuthFailed(World::rejected(ErrorCode::Forbidden, "x")),
    );
    draw(cx);
    cx.simulate_keystrokes("tab");
    cx.simulate_input("short");
    cx.simulate_keystrokes("enter");
    assert!(
        world
            .drain(|effect| matches!(effect, Effect::Register(_)).then_some(()))
            .is_empty(),
        "a malformed code is refused locally"
    );
}

#[gpui::test]
fn losing_access_shows_what_happened_with_sign_out_and_reactivation_brings_the_shell_back(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Member);
    assert!(world.on_screen(cx, "sidebar-people"), "the shell is up");
    // The server stops listing the workspace.
    let mut bootstrap = world.bootstrap(WorkspaceRole::Member);
    bootstrap.workspaces.clear();
    world.dispatch(cx, Input::BootstrapLoaded(Box::new(bootstrap)));
    draw(cx);
    assert!(
        world.on_screen(cx, "no-access"),
        "an explanation, not an empty window"
    );
    assert!(!world.on_screen(cx, "sidebar-people"), "the shell is gone");
    assert_eq!(
        world.core(cx, |core| core.access_lost().map(str::to_owned)),
        Some("Lazy".to_owned()),
        "it names the workspace"
    );
    // Check again has the keyboard: it asks for a bootstrap again and leaves the screen up.
    world.take_effects();
    cx.simulate_keystrokes("enter");
    assert!(world.on_screen(cx, "no-access"), "nothing changed yet");
    assert!(
        !world.take_effects().is_empty(),
        "Check again asks the runtime for a bootstrap"
    );
    // Sign out is the other way out.
    cx.simulate_keystrokes("tab enter");
    assert!(
        !world.on_screen(cx, "no-access"),
        "Sign out leaves the screen for the sign-in form"
    );
    assert!(world.on_screen(cx, "sign-in-submit"));
}

#[gpui::test]
fn a_session_with_no_workspace_and_no_name_to_show_still_says_so(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    // A fresh start whose first bootstrap lists no workspace (nothing was saved to name one).
    world.dispatch(
        cx,
        Input::Started(StartupState {
            session_restored: true,
            ..Default::default()
        }),
    );
    let mut bootstrap = world.bootstrap(WorkspaceRole::Member);
    bootstrap.workspaces.clear();
    world.dispatch(cx, Input::BootstrapLoaded(Box::new(bootstrap)));
    draw(cx);
    assert!(
        world.on_screen(cx, "no-access"),
        "an explanation, not an empty window"
    );
    assert_eq!(
        world.core(cx, |core| core.access_lost().map(str::to_owned)),
        None
    );
    assert!(world.on_screen(cx, "no-access-check") && world.on_screen(cx, "no-access-sign-out"));
}

#[gpui::test]
fn a_deactivated_author_is_marked_beside_the_name_in_the_timeline(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.deactivated.borrow_mut().push(world.alice.id);
    world.reach_as(cx, WorkspaceRole::Member);
    // The first message in #general is Alice's, and Alice is deactivated: still named, now marked.
    assert!(
        world.on_screen(cx, "author-deactivated-0") || world.on_screen(cx, "author-deactivated-1"),
        "the badge is beside the name"
    );
}

#[gpui::test]
fn a_later_bootstrap_that_lists_the_workspace_again_brings_the_shell_back(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Member);
    let mut bootstrap = world.bootstrap(WorkspaceRole::Member);
    bootstrap.workspaces.clear();
    world.dispatch(cx, Input::BootstrapLoaded(Box::new(bootstrap)));
    draw(cx);
    assert!(world.on_screen(cx, "no-access"));
    // An admin reactivated them: the next bootstrap lists the workspace.
    world.dispatch(
        cx,
        Input::BootstrapLoaded(Box::new(world.bootstrap(WorkspaceRole::Member))),
    );
    draw(cx);
    assert!(!world.on_screen(cx, "no-access"), "the explanation goes");
    assert!(
        world.on_screen(cx, "sidebar-people"),
        "and the shell is back"
    );
    assert_eq!(
        world.core(cx, |core| core.access_lost().map(str::to_owned)),
        None
    );
}

// ----- Creating channels under the policy -----

#[gpui::test]
fn only_admins_may_create_channels_says_why_in_the_menu(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.settings.set(WorkspaceSettings {
        channel_creation: ChannelCreationPolicy::AdminsOnly,
        ..Default::default()
    });
    world.reach_as(cx, WorkspaceRole::Member);
    assert!(!world.core(cx, |core| core.can_create_channel()));
    world.click(cx, "add-channel");
    assert!(world.on_screen(cx, "channels-menu"), "the menu opens");
    // The item is off, so the keyboard skips it: Enter opens the directory, never the form.
    cx.simulate_keystrokes("enter");
    assert!(
        !world.on_screen(cx, "create-channel-submit"),
        "Create a channel is not offered to a member"
    );
    assert!(world.on_screen(cx, "browse-list") || world.on_screen(cx, "browse-loading"));
    cx.simulate_keystrokes("escape");
    // An admin may, and the role change is live.
    world.event(
        cx,
        50,
        Event::MemberRoleChanged {
            user_id: world.me.id,
            role: WorkspaceRole::Admin,
        },
    );
    assert!(world.core(cx, |core| core.can_create_channel()));
    world.click(cx, "add-channel");
    cx.simulate_keystrokes("enter");
    assert!(
        world.on_screen(cx, "create-channel-submit"),
        "now the first item opens the form"
    );
}
