//! The first run (D29) on GPUI's test platform: an address and a code, what the server says about
//! the code, the account, the picture, and the first conversation. The server's answers are
//! played by the test; nothing is fetched.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, Modifiers, TestAppContext, VisualTestContext, WindowAppearance,
};
use lazy_client_core::{
    ActionKind, ActionOutcome, Effect, Input, PreviewFailure, RequestError, ServerCheck,
    ServerFacts, ServerInfo, ServerSource, StartupState,
};
use lazy_domain::notification::NotificationSettings;
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    ApiError, BootstrapResponse, Conversation, ConversationUserState, CreatedInvitation, ErrorCode,
    Invitation, InvitationKind, InvitationPreview, InvitationState, InviteCode, PROTOCOL_VERSION,
    ServerRegistration, User, Workspace, WorkspaceBootstrap,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::onboarding::Step;
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
    workspace: WorkspaceId,
    general: ConversationId,
}

fn start(cx: &mut TestAppContext, first_run: bool) -> (World, &mut VisualTestContext) {
    cx.update(|cx| {
        lazy_ui::init(cx, WindowAppearance::Light);
        if first_run {
            lazy_ui::onboarding::enable(cx);
        }
    });
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
        me: User {
            is_bot: false,
            deactivated: false,
            avatar_version: 0,
            status: None,
            id: UserId::generate(),
            username: Username::parse("quentin").unwrap(),
            display_name: DisplayName::parse("Quentin Fakename").unwrap(),
        },
        workspace: WorkspaceId::generate(),
        general: ConversationId::generate(),
    };
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(900.)));
    (world, cx)
}

/// Two frames: what is laid out in one is what the mouse can hit in the next.
fn draw(cx: &mut VisualTestContext) {
    for _ in 0..2 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

fn preview(kind: InvitationKind) -> InvitationPreview {
    InvitationPreview {
        kind,
        workspace_name: "Zebrafish Collective".into(),
        inviter: Some(DisplayName::parse("Alice Example").unwrap()),
        member_count: 12,
    }
}

impl World {
    fn send(&self, cx: &mut VisualTestContext, input: Input) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.run_until_parked();
        draw(cx);
    }

    fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.effects.0.borrow_mut())
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

    fn step(&self, cx: &VisualTestContext) -> Step {
        self.root.read_with(cx, |root, cx| {
            root.onboarding()
                .expect("the first-run screens")
                .read(cx)
                .step()
        })
    }

    /// A new install, started and drawn.
    fn open_new_install(&self, cx: &mut VisualTestContext) {
        self.send(cx, Input::Started(StartupState::default()));
    }

    /// Types an address and a code and presses Enter, as a person would, and returns the check
    /// that was asked for.
    fn connect(&self, cx: &mut VisualTestContext, address: &str, code: &str) -> (String, u64) {
        cx.simulate_input(address);
        cx.simulate_keystrokes("tab");
        cx.simulate_input(code);
        self.take_effects();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        self.take_effects()
            .into_iter()
            .find_map(|effect| match effect {
                Effect::CheckServer { url, generation } => Some((url, generation)),
                _ => None,
            })
            .expect("the address is checked")
    }

    /// The server answers the check, and the move to it completes. Returns what was then asked
    /// about the code, if anything.
    fn server_answers(
        &self,
        cx: &mut VisualTestContext,
        generation: u64,
        facts: Option<ServerFacts>,
        url: &str,
    ) -> Option<(String, u64)> {
        self.send(
            cx,
            Input::ServerFacts {
                url: url.to_owned(),
                facts,
            },
        );
        self.send(
            cx,
            Input::ServerChecked {
                generation,
                result: Ok(PROTOCOL_VERSION),
            },
        );
        self.send(
            cx,
            Input::ServerSwitched {
                generation,
                result: Ok(Box::default()),
            },
        );
        self.take_effects()
            .into_iter()
            .find_map(|effect| match effect {
                Effect::PreviewInvitation {
                    request,
                    generation,
                } => Some((request.code.expose().to_owned(), generation)),
                _ => None,
            })
    }

    fn bootstrap(&self) -> BootstrapResponse {
        self.bootstrap_as(WorkspaceRole::Member, "Zebrafish Collective")
    }

    fn bootstrap_as(&self, role: WorkspaceRole, name: &str) -> BootstrapResponse {
        let general = Conversation {
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
            member_count: 1,
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
                    name: name.into(),
                    settings: Default::default(),
                },
                role,
                conversations: vec![general],
                user_state: vec![ConversationUserState {
                    conversation_id: self.general,
                    favorite: false,
                    last_read: EventSeq::ZERO,
                    unread_count: 0,
                    mention_count: 0,
                }],
                members: vec![self.me.clone()],
                head: EventSeq::ZERO,
            }],
        }
    }
}

const SERVER: &str = "https://chat.example.com";
const CODE: &str = "K7QM4TXD9BWPH2VAZX3C";

fn invite_only() -> Option<ServerFacts> {
    Some(ServerFacts {
        registration: ServerRegistration::InviteOnly,
        setup_code_required: false,
    })
}

#[gpui::test]
fn a_new_install_opens_on_the_first_run_screens(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, true);
    world.open_new_install(cx);
    assert!(world.on_screen(cx, "onboarding"));
    assert!(world.on_screen(cx, "onboarding-continue"));
    assert!(
        world.on_screen(cx, "onboarding-panel"),
        "the brand panel, in a wide window"
    );
    assert!(!world.on_screen(cx, "sign-in-card"));
    assert_eq!(world.step(cx), Step::Connect);
}

#[gpui::test]
fn without_the_first_run_screens_the_sign_in_card_is_first_as_it_always_was(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx, false);
    world.open_new_install(cx);
    assert!(world.on_screen(cx, "sign-in-card"));
    assert!(!world.on_screen(cx, "onboarding"));
    assert!(!world.on_screen(cx, "join-instead"));
}

#[gpui::test]
fn someone_with_a_saved_server_gets_the_sign_in_card_and_can_choose_to_join_instead(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx, true);
    world.send(
        cx,
        Input::Started(StartupState {
            server: ServerInfo {
                url: SERVER.into(),
                source: ServerSource::Saved,
                trying: None,
                check: ServerCheck::Unchecked,
            },
            ..Default::default()
        }),
    );
    assert!(world.on_screen(cx, "sign-in-card"));
    assert!(!world.on_screen(cx, "onboarding"));
    world.click(cx, "join-instead");
    assert!(world.on_screen(cx, "onboarding"));
    assert!(!world.on_screen(cx, "sign-in-card"));
    // And back, for someone who does have an account.
    world.click(cx, "onboarding-have-account");
    assert!(world.on_screen(cx, "sign-in-card"));
}

#[gpui::test]
fn the_whole_way_from_an_address_to_the_first_conversation(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, true);
    world.open_new_install(cx);

    // Step 1: the address and the code, pasted the way they were shown.
    let (url, generation) = world.connect(cx, "chat.example.com", "k7qm-4txd-9bwp-h2va-zx3c");
    assert_eq!(url, SERVER);
    assert!(
        world.on_screen(cx, "onboarding-address-status"),
        "Checking…"
    );
    let (code, asked) = world
        .server_answers(cx, generation, invite_only(), SERVER)
        .expect("the server is asked what the code is for");
    assert_eq!(code, CODE, "in its plain form");

    // Step 2: who invited them.
    world.send(
        cx,
        Input::InvitationPreviewed {
            generation: asked,
            result: Ok(preview(InvitationKind::Invitation)),
        },
    );
    assert_eq!(world.step(cx), Step::Invited);
    assert!(world.on_screen(cx, "onboarding-join"));
    assert!(world.on_screen(cx, "onboarding-support"));

    // Step 3: "Join workspace" has the keyboard.
    cx.simulate_keystrokes("enter");
    assert_eq!(world.step(cx), Step::Account);
    cx.simulate_input("Quentin Fakename");
    cx.simulate_keystrokes("tab tab");
    cx.simulate_input("correct horse");
    world.take_effects();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let registered = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Register(request) => Some(request),
            _ => None,
        })
        .expect("the account is created");
    assert_eq!(
        registered.username.as_str(),
        "quentin",
        "suggested from the name"
    );
    assert_eq!(registered.display_name.as_str(), "Quentin Fakename");
    assert_eq!(
        registered.invite_code.as_ref().map(|c| c.expose()),
        Some(CODE)
    );

    // A name that is taken: said, with a way out, and the form is still there.
    world.send(
        cx,
        Input::AuthFailed(RequestError::Rejected(ApiError::new(
            ErrorCode::UsernameTaken,
            "that username is taken",
        ))),
    );
    assert!(world.on_screen(cx, "onboarding-problem"));
    assert_eq!(world.step(cx), Step::Account);

    // Again, and it works: the shell is not shown yet, the picture is asked for.
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    world.send(
        cx,
        Input::Authenticated {
            user: world.me.clone(),
        },
    );
    world.send(cx, Input::BootstrapLoaded(Box::new(world.bootstrap())));
    world.send(cx, Input::Clock(Timestamp::now()));
    assert_eq!(world.step(cx), Step::Picture);
    assert!(world.on_screen(cx, "onboarding-choose-picture"));
    assert!(
        world.root.read_with(cx, |root, _| root.shell().is_none()),
        "the conversation waits behind the last two screens"
    );

    // Steps 4 and 5.
    world.click(cx, "onboarding-skip");
    assert_eq!(world.step(cx), Step::Done);
    assert!(world.on_screen(cx, "onboarding-tip"));
    world.click(cx, "onboarding-open");
    assert!(
        world.on_screen(cx, "sidebar-menu"),
        "the first conversation"
    );
    assert!(!world.on_screen(cx, "onboarding"));
}

#[gpui::test]
fn a_code_the_server_refuses_is_said_under_the_code_and_nothing_moves_on(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, true);
    world.open_new_install(cx);
    let (_, generation) = world.connect(cx, "chat.example.com", CODE);
    let (_, asked) = world
        .server_answers(cx, generation, invite_only(), SERVER)
        .expect("asked");
    world.send(
        cx,
        Input::InvitationPreviewed {
            generation: asked,
            result: Err(PreviewFailure::Invalid),
        },
    );
    assert_eq!(world.step(cx), Step::Connect);
    assert!(world.on_screen(cx, "onboarding-code-problem"));
    // Typing again forgets the complaint.
    cx.simulate_input("x");
    assert!(!world.on_screen(cx, "onboarding-code-problem"));
}

#[gpui::test]
fn a_server_open_to_anyone_needs_no_code_and_an_invitation_only_one_asks_for_it(
    cx: &mut TestAppContext,
) {
    // Open: the address alone goes on to the account.
    let (world, cx) = start(cx, true);
    world.open_new_install(cx);
    cx.simulate_input("chat.example.com");
    world.take_effects();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let generation = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::CheckServer { generation, .. } => Some(generation),
            _ => None,
        })
        .expect("checked");
    let asked = world.server_answers(
        cx,
        generation,
        Some(ServerFacts {
            registration: ServerRegistration::Open,
            setup_code_required: false,
        }),
        SERVER,
    );
    assert_eq!(asked, None, "there is no code to ask about");
    assert_eq!(world.step(cx), Step::Account);
}

#[gpui::test]
fn an_invitation_only_server_without_a_code_says_a_code_is_needed(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, true);
    world.open_new_install(cx);
    cx.simulate_input("chat.example.com");
    world.take_effects();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let generation = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::CheckServer { generation, .. } => Some(generation),
            _ => None,
        })
        .expect("checked");
    world.server_answers(cx, generation, invite_only(), SERVER);
    assert_eq!(world.step(cx), Step::Connect);
    assert!(world.on_screen(cx, "onboarding-code-problem"));
}

#[gpui::test]
fn a_server_too_old_to_say_what_a_code_is_for_goes_straight_to_the_account(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx, true);
    world.open_new_install(cx);
    let (_, generation) = world.connect(cx, "chat.example.com", CODE);
    let (_, asked) = world
        .server_answers(cx, generation, None, SERVER)
        .expect("asked");
    world.send(
        cx,
        Input::InvitationPreviewed {
            generation: asked,
            result: Err(PreviewFailure::Unsupported),
        },
    );
    assert_eq!(
        world.step(cx),
        Step::Account,
        "the code is checked with the account"
    );
}

#[gpui::test]
fn an_address_that_does_not_answer_is_said_and_the_person_stays(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, true);
    world.open_new_install(cx);
    let (_, generation) = world.connect(cx, "chat.example.com", CODE);
    world.send(
        cx,
        Input::ServerChecked {
            generation,
            result: Err("connection failed".into()),
        },
    );
    assert_eq!(world.step(cx), Step::Connect);
    assert!(world.on_screen(cx, "onboarding-address-status"));
    // Continue is there to try again.
    assert!(world.on_screen(cx, "onboarding-continue"));
    world.take_effects();
    world.click(cx, "onboarding-continue");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::CheckServer { .. })),
        "asked again"
    );
}

#[gpui::test]
fn the_first_screen_keeps_the_keyboard_in_its_own_fields_and_buttons(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, true);
    world.open_new_install(cx);
    // Tab visits the two fields and the two buttons and comes round; it never leaves the screen.
    for _ in 0..8 {
        cx.simulate_keystrokes("tab");
        assert!(world.on_screen(cx, "onboarding-continue"));
    }
    cx.simulate_keystrokes("shift-tab");
    assert_eq!(world.step(cx), Step::Connect);
}

#[gpui::test]
fn tab_walks_a_screen_of_buttons_and_enter_presses_the_one_it_is_on(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, true);
    world.owner_at_picture(cx);
    // "Choose a picture" has the keyboard; Tab goes on to "Skip for now", which Enter presses.
    cx.simulate_keystrokes("tab");
    assert_eq!(world.step(cx), Step::Picture);
    cx.simulate_keystrokes("enter");
    assert_eq!(world.step(cx), Step::Workspace);
}

#[test]
fn the_invitation_code_is_read_in_one_place_only() {
    // The screens hand the code to the core and nothing else; anything that formats it (a label,
    // a tooltip, a log line) would leak it, so this reads the source and counts.
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/onboarding.rs"),
    )
    .expect("the first-run source");
    let code = source.split("#[cfg(test)]").next().unwrap();
    assert_eq!(
        code.matches("code.read(cx).text()").count(),
        1,
        "the code's text is read in `code_text` and nowhere else"
    );
    assert_eq!(code.matches("tracing::").count(), 0);
    assert!(
        code.contains("\"Invitation code\""),
        "its name is its label, never its content"
    );
}

// ----- The owner: the setup code, then a name for the workspace and the first invitation -----

fn setup_server() -> Option<ServerFacts> {
    Some(ServerFacts {
        registration: ServerRegistration::InviteOnly,
        setup_code_required: true,
    })
}

impl World {
    fn invitation(&self) -> Invitation {
        Invitation {
            id: lazy_domain::InvitationId::generate(),
            workspace_id: self.workspace,
            created_by: self.me.id,
            created_at: Timestamp::now(),
            expires_at: Timestamp::from(time::OffsetDateTime::now_utc() + time::Duration::days(7)),
            max_uses: Some(25),
            uses: 0,
            state: InvitationState::Active,
        }
    }

    /// Joins with the setup code: the owner is on the picture step.
    fn owner_at_picture(&self, cx: &mut VisualTestContext) {
        self.open_new_install(cx);
        let (_, generation) = self.connect(cx, "chat.example.com", CODE);
        let (_, asked) = self
            .server_answers(cx, generation, setup_server(), SERVER)
            .expect("asked");
        self.send(
            cx,
            Input::InvitationPreviewed {
                generation: asked,
                result: Ok(preview(InvitationKind::Setup)),
            },
        );
        assert_eq!(self.step(cx), Step::Invited);
        cx.simulate_keystrokes("enter");
        assert_eq!(self.step(cx), Step::Account);
        cx.simulate_input("Quentin Fakename");
        cx.simulate_keystrokes("tab tab");
        cx.simulate_input("correct horse");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        self.send(
            cx,
            Input::Authenticated {
                user: self.me.clone(),
            },
        );
        self.send(
            cx,
            Input::BootstrapLoaded(Box::new(self.bootstrap_as(WorkspaceRole::Owner, "Lazy"))),
        );
        self.send(cx, Input::Clock(Timestamp::now()));
        assert_eq!(self.step(cx), Step::Picture);
    }

    /// Joins with the setup code and skips the picture: the owner is on the step that names the
    /// workspace.
    fn owner_at_naming(&self, cx: &mut VisualTestContext) {
        self.owner_at_picture(cx);
        self.click(cx, "onboarding-skip");
        self.take_effects();
    }

    fn renamed(&self, name: &str) -> Input {
        Input::ActionResult {
            kind: ActionKind::UpdateWorkspace,
            result: Ok(ActionOutcome::Workspace(Workspace {
                id: self.workspace,
                name: name.into(),
                settings: Default::default(),
            })),
        }
    }

    fn invitation_made(&self) -> Input {
        Input::ActionResult {
            kind: ActionKind::CreateInvitation,
            result: Ok(ActionOutcome::InvitationCreated(CreatedInvitation {
                invitation: self.invitation(),
                code: InviteCode::new(CODE),
            })),
        }
    }
}

fn clipboard(cx: &VisualTestContext) -> String {
    cx.read_from_clipboard()
        .and_then(|item| item.text())
        .unwrap_or_default()
}

#[gpui::test]
fn the_owner_names_the_workspace_and_is_handed_the_first_invitation_to_send(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx, true);
    world.owner_at_naming(cx);
    assert!(
        world.root.read_with(cx, |root, cx| root
            .onboarding()
            .expect("the first-run screens")
            .read(cx)
            .is_owner()),
        "joined with the setup code"
    );

    // The picture was the fourth of six steps; the next asks what the workspace is called.
    assert_eq!(world.step(cx), Step::Workspace);
    assert!(world.on_screen(cx, "onboarding-workspace-continue"));
    cx.simulate_input("Acme Design");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let renamed = world.take_effects();
    let Some(Effect::UpdateWorkspace { request, .. }) = renamed
        .iter()
        .find(|effect| matches!(effect, Effect::UpdateWorkspace { .. }))
    else {
        panic!("the workspace is renamed: {renamed:?}");
    };
    assert_eq!(
        request.name.as_ref().map(|name| name.as_str()),
        Some("Acme Design")
    );
    assert!(request.settings.is_none(), "nothing else is changed");
    assert_eq!(world.step(cx), Step::Workspace, "until the server agrees");

    // It agrees: the first invitation is asked for, a week, for up to 25 people.
    world.send(cx, world.renamed("Acme Design"));
    assert_eq!(world.step(cx), Step::Share);
    let asked = world.take_effects();
    let Some(Effect::CreateInvitation { request, .. }) = asked
        .iter()
        .find(|effect| matches!(effect, Effect::CreateInvitation { .. }))
    else {
        panic!("the first invitation is asked for: {asked:?}");
    };
    assert_eq!(request.expires_in_secs, Some(7 * 24 * 60 * 60));
    assert_eq!(request.max_uses, Some(25));
    assert!(world.on_screen(cx, "onboarding-share-waiting"));
    assert!(!world.on_screen(cx, "onboarding-message"));

    // The code arrives: the message to send, and the keyboard on its button.
    world.send(cx, world.invitation_made());
    assert!(world.on_screen(cx, "onboarding-message"));
    assert!(world.on_screen(cx, "onboarding-message-server"));
    assert!(!world.on_screen(cx, "onboarding-share-waiting"));
    cx.simulate_keystrokes("enter");
    let message = clipboard(cx);
    assert!(message.starts_with("Join us on Lazy."), "{message}");
    assert!(message.contains("Server: chat.example.com"), "{message}");
    assert!(
        message.contains("Invitation code: K7QM-4TXD-9BWP-H2VA-ZX3C"),
        "{message}"
    );
    assert!(
        message.contains(lazy_client_core::RELEASES_URL),
        "{message}"
    );
    assert!(world.on_screen(cx, "onboarding-copied"));

    // Continue forgets the code (it is shown once) and finishes.
    world.click(cx, "onboarding-share-continue");
    assert_eq!(world.step(cx), Step::Done);
    assert!(
        world
            .model
            .read_with(cx, |model, _| model.core().invitations().created.is_none()),
        "the code goes when the step does"
    );
    world.click(cx, "onboarding-open");
    assert!(world.on_screen(cx, "sidebar-menu"));
}

#[gpui::test]
fn an_owner_who_keeps_the_name_or_skips_it_goes_straight_to_the_invitation(
    cx: &mut TestAppContext,
) {
    // Skipping.
    let (world, cx) = start(cx, true);
    world.owner_at_naming(cx);
    assert_eq!(world.step(cx), Step::Workspace);
    assert!(world.on_screen(cx, "onboarding-workspace-skip"));
    world.click(cx, "onboarding-workspace-skip");
    assert_eq!(world.step(cx), Step::Share);
    assert!(
        world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::CreateInvitation { .. }))
    );
}

#[gpui::test]
fn the_name_it_already_has_is_not_sent_again(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, true);
    world.owner_at_naming(cx);
    cx.simulate_input("Lazy");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(world.step(cx), Step::Share);
    let sent = world.take_effects();
    assert!(
        !sent
            .iter()
            .any(|effect| matches!(effect, Effect::UpdateWorkspace { .. })),
        "{sent:?}"
    );
    assert!(
        sent.iter()
            .any(|effect| matches!(effect, Effect::CreateInvitation { .. }))
    );
}

#[gpui::test]
fn an_empty_name_cannot_be_sent_and_a_refused_one_is_said_and_can_be_changed(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx, true);
    world.owner_at_naming(cx);
    // Nothing typed: Enter does nothing.
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(
        !world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::UpdateWorkspace { .. }))
    );
    assert_eq!(world.step(cx), Step::Workspace);

    cx.simulate_input("Acme Design");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    world.take_effects();
    world.send(
        cx,
        Input::ActionResult {
            kind: ActionKind::UpdateWorkspace,
            result: Err(RequestError::Rejected(ApiError::new(
                ErrorCode::Forbidden,
                "not allowed",
            ))),
        },
    );
    assert_eq!(world.step(cx), Step::Workspace);
    assert!(world.on_screen(cx, "onboarding-problem"));
    // Typing again forgets the complaint, and sending again asks again.
    cx.simulate_input("s");
    assert!(!world.on_screen(cx, "onboarding-problem"));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(
        world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::UpdateWorkspace { .. }))
    );
}

#[gpui::test]
fn an_invitation_that_could_not_be_made_says_so_and_can_be_asked_for_again(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx, true);
    world.owner_at_naming(cx);
    world.click(cx, "onboarding-workspace-skip");
    world.take_effects();
    world.send(
        cx,
        Input::ActionResult {
            kind: ActionKind::CreateInvitation,
            result: Err(RequestError::Rejected(ApiError::new(
                ErrorCode::Forbidden,
                "not allowed",
            ))),
        },
    );
    assert!(world.on_screen(cx, "onboarding-problem"));
    assert!(world.on_screen(cx, "onboarding-share-retry"));
    assert!(!world.on_screen(cx, "onboarding-message"));
    world.click(cx, "onboarding-share-retry");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::CreateInvitation { .. })),
        "it asks again"
    );
    // And it works the second time.
    world.send(cx, world.invitation_made());
    assert!(world.on_screen(cx, "onboarding-message"));
    assert!(!world.on_screen(cx, "onboarding-problem"));
}

#[gpui::test]
fn the_code_in_the_owners_message_cannot_be_edited(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, true);
    world.owner_at_naming(cx);
    world.click(cx, "onboarding-workspace-skip");
    world.send(cx, world.invitation_made());
    // Tab to the code field (the keyboard starts on the copy button, so shift-tab goes back).
    cx.simulate_keystrokes("shift-tab");
    cx.simulate_input("zzz");
    cx.simulate_keystrokes("tab enter");
    let message = clipboard(cx);
    assert!(
        message.contains("K7QM-4TXD-9BWP-H2VA-ZX3C") && !message.contains("zzz"),
        "{message}"
    );
}
