//! Slice 3H/3I on GPUI's test platform: Help, Diagnostics… shows and copies a report that has no
//! secrets and no content. A scenario puts distinctive strings everywhere a secret or content can
//! be (the server's address, a password, an invitation code, a message, a draft, a search, names,
//! the text of a refusal), and the report is scanned for every one of them.

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
use lazy_ui::commands::CommandId;
use lazy_ui::dialogs::{DesktopInfo, set_desktop_info};
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
        me: person("quentin", "Quentin Fakename"),
        bob: person("rosalind", "Rosalind Notarealperson"),
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

/// The harness is the same shape as `tests/commands.rs`; not every helper is used here.
#[allow(dead_code)]
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
                    name: "Zebrafish Collective".into(),
                    settings: Default::default(),
                },
                role: self.role,
                conversations: vec![
                    self.channel(self.general, "xyzzy-lounge"),
                    self.channel(self.design, "plugh-corner"),
                    self.channel(self.engineering, "frobozz-annex"),
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
                        document: MessageDocument::from_markdown("xylophone-confidential-message")
                            .unwrap(),
                        created_at: Timestamp::from(at),
                        revision: 1,
                        seq: EventSeq::new(1).unwrap(),
                        edited_at: None,
                        reactions: vec![],
                        reactions_as_of: EventSeq::ZERO,
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

const SECRETS: &[&str] = &[
    // The server's address carried credentials.
    "hunter2-urlpass",
    "tok3n-in-the-url",
    "alice-url-user",
    // Typed into the program.
    "correct-horse-battery-staple",
    "lazy-secretinvitecode1234567",
    "my-private-draft-text",
    "needle-in-a-search-query",
    // What was said and who said it.
    "xylophone-confidential-message",
    "Quentin Fakename",
    "Rosalind Notarealperson",
    "Zebrafish Collective",
    "xyzzy-lounge",
    "plugh-corner",
    "frobozz-annex",
    // What the server said when it refused.
    "tomato-secret-refusal-text",
];

fn desktop_info() -> DesktopInfo {
    DesktopInfo::new(
        "1.2.3-test",
        "linux x86_64",
        "https://alice-url-user:hunter2-urlpass@chat.example.com:8443/api?token=tok3n-in-the-url#frag",
        "/home/someone/.local/share/lazy",
        "/home/someone/.local/share/lazy/logs",
    )
}

/// Everything a secret or content can reach: sign-in, an invitation, a draft, a search, a
/// message, and a refusal with words in it.
fn scenario(world: &World, cx: &mut VisualTestContext) {
    cx.update(|_, cx| set_desktop_info(cx, desktop_info()));
    world.dispatch(
        cx,
        Input::Started(StartupState {
            session_restored: false,
            ..Default::default()
        }),
    );
    world.dispatch(
        cx,
        Input::SignIn {
            username: "quentin".into(),
            password: "correct-horse-battery-staple".into(),
        },
    );
    world.reach(cx);
    // An invitation code typed in and refused.
    world.dispatch(
        cx,
        Input::AcceptInvitation {
            code: "lazy-secretinvitecode1234567".into(),
        },
    );
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: lazy_client_core::ActionKind::AcceptInvitation,
            result: Err(lazy_client_core::RequestError::Rejected(
                lazy_protocol::ApiError::new(
                    lazy_protocol::ErrorCode::NotFound,
                    "tomato-secret-refusal-text",
                ),
            )),
        },
    );
    // A draft in the composer and a search.
    cx.simulate_input("my-private-draft-text");
    world.dispatch(cx, Input::OpenSearch);
    world.dispatch(
        cx,
        Input::SetSearchQuery {
            text: "needle-in-a-search-query".into(),
        },
    );
}

fn open_diagnostics(
    world: &World,
    cx: &mut VisualTestContext,
) -> Entity<lazy_ui::dialogs::DiagnosticsDialog> {
    world.run(cx, CommandId::Diagnostics);
    draw(cx);
    let shell = world.shell(cx);
    shell
        .read_with(cx, |shell, _| shell.diagnostics_dialog())
        .expect("Help, Diagnostics… opened")
}

#[gpui::test]
fn the_report_has_what_the_desktop_and_the_core_know_and_none_of_the_secrets(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    scenario(&world, cx);
    let dialog = open_diagnostics(&world, cx);
    let report = cx.update(|_, cx| dialog.read(cx).report(cx));
    // What it is for.
    for wanted in [
        "application: 1.2.3-test",
        "operating system: linux x86_64",
        "server: https://chat.example.com:8443/api",
        "profile folder: /home/someone/.local/share/lazy",
        "log folder: /home/someone/.local/share/lazy/logs",
        "protocol: ",
        "session: ready",
        "workspace 1",
    ] {
        assert!(
            report.contains(wanted),
            "the report lacks {wanted:?}:\n{report}"
        );
    }
    // What it must not have.
    for secret in SECRETS {
        assert!(
            !report.contains(secret),
            "the report holds {secret:?}:\n{report}"
        );
    }
    // The refusal shows as a code, never its words.
    assert!(
        report.contains("not_found"),
        "the refusal's code is listed (and only the code):\n{report}"
    );
}

#[gpui::test]
fn copy_puts_the_same_plain_text_on_the_clipboard(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    scenario(&world, cx);
    let dialog = open_diagnostics(&world, cx);
    assert!(!dialog.read_with(cx, |dialog, _| dialog.copied()));
    // "Copy diagnostics" has the keyboard; Enter copies.
    cx.simulate_keystrokes("enter");
    assert!(dialog.read_with(cx, |dialog, _| dialog.copied()));
    let copied = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("plain text on the clipboard");
    let report = cx.update(|_, cx| dialog.read(cx).report(cx));
    assert_eq!(copied, report);
    for secret in SECRETS {
        assert!(!copied.contains(secret), "the clipboard holds {secret:?}");
    }
    // The log folder is shown as a path, not as a link that starts something.
    assert!(world.on_screen(cx, "diagnostics-logs"));
    cx.simulate_keystrokes("escape");
    assert!(
        world
            .shell(cx)
            .read_with(cx, |shell, _| shell.diagnostics_dialog().is_none())
    );
}

#[gpui::test]
fn the_dialog_is_reached_from_the_menu_and_the_palette_and_the_keyboard_stays_inside(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx, WorkspaceRole::Member);
    scenario(&world, cx);
    // Help, Diagnostics… in the command menu.
    world.click(cx, "sidebar-menu");
    world.click(cx, "command-menu-tab-Help");
    world.click(cx, "command-menu-Diagnostics");
    draw(cx);
    let shell = world.shell(cx);
    assert!(shell.read_with(cx, |shell, _| shell.diagnostics_dialog().is_some()));
    // One Tab stop: Tab stays on the copy button.
    cx.simulate_keystrokes("tab tab shift-tab");
    cx.simulate_keystrokes("enter");
    let dialog = shell
        .read_with(cx, |shell, _| shell.diagnostics_dialog())
        .unwrap();
    assert!(
        dialog.read_with(cx, |dialog, _| dialog.copied()),
        "the keyboard stayed on the button"
    );
    cx.simulate_keystrokes("escape");
    // And from the palette.
    cx.simulate_keystrokes("ctrl-shift-p");
    cx.simulate_input("diag");
    cx.simulate_keystrokes("enter");
    draw(cx);
    assert!(shell.read_with(cx, |shell, _| shell.diagnostics_dialog().is_some()));
}

#[gpui::test]
fn without_the_desktops_info_the_report_says_unknown_rather_than_nothing(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, WorkspaceRole::Owner);
    world.reach(cx);
    let dialog = open_diagnostics(&world, cx);
    let report = cx.update(|_, cx| dialog.read(cx).report(cx));
    assert!(report.contains("application: unknown"), "{report}");
    assert!(report.contains("log folder: unknown"), "{report}");
}
