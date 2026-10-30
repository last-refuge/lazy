//! Updates (D28) on GPUI's test platform: the bar across the top of the window, Help, Check for
//! updates…, and what each button asks of the host. The host here is a recorder; nothing is
//! fetched or installed. The rules that choose the button and the words are unit-tested in
//! `lazy_ui::updates`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, Modifiers, TestAppContext, VisualTestContext, WindowAppearance,
};
use lazy_client_core::{
    AvailableUpdate, Effect, Input, Package, StartupState, UpdateFailure, UpdateStatus, Version,
    parse_checksums,
};
use lazy_domain::notification::NotificationSettings;
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    BootstrapResponse, Conversation, ConversationUserState, PROTOCOL_VERSION, User, Workspace,
    WorkspaceBootstrap,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::commands::CommandId;
use lazy_ui::root_view::RootView;
use lazy_ui::updates::{self, Offer, UpdateHost, UpdateModel};

/// The core's effects go nowhere: nothing here is about the client core.
struct Discard;

impl EffectSink for Discard {
    fn execute(&self, _effects: Vec<Effect>) {}
}

/// Records what the UI asks of the desktop.
struct FakeHost {
    calls: RefCell<Vec<String>>,
    automatic: Cell<bool>,
    can_install: Cell<bool>,
    install_result: Cell<Result<(), UpdateFailure>>,
}

impl FakeHost {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            calls: RefCell::default(),
            automatic: Cell::new(true),
            can_install: Cell::new(true),
            install_result: Cell::new(Ok(())),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    fn record(&self, call: &str) {
        self.calls.borrow_mut().push(call.to_owned());
    }
}

impl UpdateHost for FakeHost {
    fn can_install(&self) -> bool {
        self.can_install.get()
    }

    fn automatic(&self) -> bool {
        self.automatic.get()
    }

    fn set_automatic(&self, automatic: bool) {
        self.automatic.set(automatic);
        self.record(if automatic {
            "automatic-on"
        } else {
            "automatic-off"
        });
    }

    fn check_now(&self) {
        self.record("check");
    }

    fn download(&self) {
        self.record("download");
    }

    fn install(&self) -> Result<(), UpdateFailure> {
        self.record("install");
        self.install_result.get()
    }
}

struct World {
    model: Entity<AppModel>,
    root: Entity<RootView>,
    host: Rc<FakeHost>,
    updates: Option<Entity<UpdateModel>>,
    me: User,
    workspace: WorkspaceId,
    general: ConversationId,
}

fn start(cx: &mut TestAppContext, with_updater: bool) -> (World, &mut VisualTestContext) {
    let host = FakeHost::new();
    let updates = cx.update(|cx| {
        lazy_ui::init(cx, WindowAppearance::Light);
        with_updater.then(|| updates::install(cx, host.clone(), "0.5.0"))
    });
    let model = cx.new(|_| AppModel::new(Rc::new(Discard)));
    let (root, cx) = cx.add_window_view({
        let model = model.clone();
        move |window, cx| RootView::new(model, window, cx)
    });
    let world = World {
        model,
        root,
        host,
        updates,
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
    (world, cx)
}

/// Two frames: what is laid out in one is what the mouse can hit in the next.
fn draw(cx: &mut VisualTestContext) {
    for _ in 0..2 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

fn release(version: &str) -> AvailableUpdate {
    let package = Package::LinuxDeb;
    let name = package.file_name(&Version::parse(version).unwrap());
    AvailableUpdate::newest(
        &parse_checksums(&format!("{}  {name}\n", "ab".repeat(32))),
        package,
        &Version::new(0, 5, 0),
        "https://example.test/releases",
    )
    .expect("newer than 0.5.0")
}

impl World {
    fn dispatch(&self, cx: &mut VisualTestContext, input: Input) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.run_until_parked();
    }

    /// Signed in with one channel.
    fn reach(&self, cx: &mut VisualTestContext) {
        cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(900.)));
        self.dispatch(
            cx,
            Input::Started(StartupState {
                session_restored: true,
                ..Default::default()
            }),
        );
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
        self.dispatch(
            cx,
            Input::BootstrapLoaded(Box::new(BootstrapResponse {
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
                    role: WorkspaceRole::Member,
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
            })),
        );
        self.dispatch(cx, Input::Clock(Timestamp::now()));
        draw(cx);
    }

    /// The updater reports `status`.
    fn report(&self, cx: &mut VisualTestContext, status: UpdateStatus) {
        let updates = self.updates.clone().expect("an updater");
        updates.update(cx, |model, cx| model.set_status(status, cx));
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

    fn run(&self, cx: &mut VisualTestContext, id: CommandId) {
        let shell = self
            .root
            .read_with(cx, |root, _| root.shell().expect("the shell").clone());
        shell.update_in(cx, |shell, window, cx| shell.run_command(id, window, cx));
        draw(cx);
    }

    fn status(&self, cx: &VisualTestContext) -> UpdateStatus {
        let updates = self.updates.clone().expect("an updater");
        updates.read_with(cx, |model, _| model.status().clone())
    }
}

#[gpui::test]
fn the_bar_follows_a_release_from_found_to_installed(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, true);
    world.reach(cx);
    let found = release("0.6.0");

    // Nothing to say: nothing is drawn, whatever the updater has checked.
    assert!(!world.on_screen(cx, "update-bar"));
    world.report(cx, UpdateStatus::Checking);
    world.report(cx, UpdateStatus::UpToDate);
    assert!(
        !world.on_screen(cx, "update-bar"),
        "checks that find nothing are silent"
    );

    // A release: the bar offers it, and the button asks the host to fetch it.
    world.report(cx, UpdateStatus::Available(found.clone()));
    assert!(world.on_screen(cx, "update-bar"));
    assert!(world.on_screen(cx, "update-bar-later"));
    world.click(cx, "update-bar-act");
    assert_eq!(world.host.calls(), ["download"]);

    // While it comes there is nothing to press and nothing to put off.
    world.report(
        cx,
        UpdateStatus::Downloading {
            update: found.clone(),
            received: 10,
            total: Some(100),
        },
    );
    assert!(world.on_screen(cx, "update-bar-act"));
    assert!(!world.on_screen(cx, "update-bar-later"));
    world.click(cx, "update-bar-act");
    assert_eq!(
        world.host.calls(),
        ["download"],
        "a button that waits does nothing"
    );

    // Ready: the button restarts; a host that cannot start the installer is told as a failure.
    let ready = UpdateStatus::Ready {
        update: found.clone(),
        file: "lazy-chat_0.6.0_amd64.deb".into(),
    };
    world.report(cx, ready.clone());
    world.host.install_result.set(Err(UpdateFailure::Local));
    world.click(cx, "update-bar-act");
    assert_eq!(world.host.calls(), ["download", "install"]);
    assert_eq!(
        world.status(cx),
        UpdateStatus::Failed {
            update: Some(found.clone()),
            failure: UpdateFailure::Local
        }
    );
    assert!(
        world.on_screen(cx, "update-bar"),
        "the failure is on the bar"
    );

    // Trying again fetches again (the runtime reuses the file if it is whole).
    world.click(cx, "update-bar-act");
    assert_eq!(world.host.calls(), ["download", "install", "download"]);

    // And a host that starts the installer is asked once.
    world.report(cx, ready);
    world.host.install_result.set(Ok(()));
    world.click(cx, "update-bar-act");
    assert_eq!(
        world.host.calls(),
        ["download", "install", "download", "install"]
    );
}

#[gpui::test]
fn later_puts_a_release_off_until_there_is_a_newer_one_or_the_person_asks(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, true);
    world.reach(cx);
    world.report(cx, UpdateStatus::Available(release("0.6.0")));
    assert!(world.on_screen(cx, "update-bar"));

    world.click(cx, "update-bar-later");
    assert!(!world.on_screen(cx, "update-bar"));
    assert!(
        world.host.calls().is_empty(),
        "Later asks nothing of the host"
    );

    // The same release found again stays put off.
    world.report(cx, UpdateStatus::Available(release("0.6.0")));
    assert!(!world.on_screen(cx, "update-bar"));

    // A newer one is news.
    world.report(cx, UpdateStatus::Available(release("0.6.1")));
    assert!(world.on_screen(cx, "update-bar"));

    // Asking for a check brings back a release that was put off.
    world.click(cx, "update-bar-later");
    assert!(!world.on_screen(cx, "update-bar"));
    let updates = world.updates.clone().unwrap();
    updates.update(cx, |model, cx| model.check_now(cx));
    assert!(world.on_screen(cx, "update-bar"));
    assert_eq!(world.host.calls(), ["check"]);
}

#[gpui::test]
fn the_bar_goes_above_any_screen_and_a_screen_without_one_is_laid_out_as_before(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx, true);
    // Before anything has loaded (the splash).
    let before = {
        draw(cx);
        cx.debug_bounds("dialog-scrim")
    };
    assert!(before.is_none());
    world.report(cx, UpdateStatus::Available(release("0.6.0")));
    assert!(world.on_screen(cx, "update-bar"));
    let bar = cx.debug_bounds("update-bar").expect("the bar");
    assert!(
        f32::from(bar.origin.y).abs() < 1.0,
        "the bar is at the top of the window"
    );
    // Signed in: the same bar, and the shell below it.
    world.reach(cx);
    assert!(world.on_screen(cx, "update-bar"));
    assert!(world.on_screen(cx, "sidebar-menu"));
    let shell_top = cx.debug_bounds("sidebar-menu").expect("the menu").origin.y;
    assert!(
        shell_top > bar.size.height,
        "the shell starts below the bar"
    );
}

#[gpui::test]
fn a_release_that_cannot_replace_this_install_sends_the_person_to_its_page(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx, true);
    world.reach(cx);
    world.host.can_install.set(false);
    let found = release("0.6.0");
    world.report(cx, UpdateStatus::Available(found.clone()));
    let updates = world.updates.clone().unwrap();
    assert_eq!(
        updates.read_with(cx, |model, _| model.offer()),
        Offer::Page(found.release_url.clone())
    );
    assert!(world.on_screen(cx, "update-bar-act"));
}

#[gpui::test]
fn help_check_for_updates_asks_for_a_check_and_shows_what_it_finds(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, true);
    world.reach(cx);
    world.run(cx, CommandId::CheckForUpdates);
    let shell = world
        .root
        .read_with(cx, |root, _| root.shell().expect("the shell").clone());
    assert!(shell.read_with(cx, |shell, _| shell.updates_dialog().is_some()));
    assert_eq!(world.host.calls(), ["check"], "opening it checks");

    // The answer, as the updater reports it.
    for selector in [
        "updates-headline",
        "updates-current",
        "updates-primary",
        "updates-automatic",
    ] {
        assert!(world.on_screen(cx, selector), "{selector}");
    }
    assert!(!world.on_screen(cx, "updates-notes"), "no release yet");
    assert!(!world.on_screen(cx, "updates-progress"));
    world.report(cx, UpdateStatus::UpToDate);
    world.report(cx, UpdateStatus::Available(release("0.6.0")));
    assert!(world.on_screen(cx, "updates-notes"));
    world.report(
        cx,
        UpdateStatus::Downloading {
            update: release("0.6.0"),
            received: 25,
            total: Some(100),
        },
    );
    assert!(world.on_screen(cx, "updates-progress"));

    // The main button does what the bar's does.
    world.report(cx, UpdateStatus::Available(release("0.6.0")));
    world.click(cx, "updates-primary");
    assert_eq!(world.host.calls(), ["check", "download"]);

    // The switch for the automatic checks reaches the host.
    assert!(world.host.automatic.get());
    world.click(cx, "updates-automatic");
    assert!(!world.host.automatic.get());
    assert_eq!(
        world.host.calls().last().map(String::as_str),
        Some("automatic-off")
    );
    world.click(cx, "updates-automatic");
    assert!(world.host.automatic.get());

    // Escape closes it.
    cx.simulate_keystrokes("escape");
    assert!(shell.read_with(cx, |shell, _| shell.updates_dialog().is_none()));
}

#[gpui::test]
fn the_dialog_is_reached_from_the_help_menu(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, true);
    world.reach(cx);
    world.click(cx, "sidebar-menu");
    world.click(cx, "command-menu-tab-Help");
    world.click(cx, "command-menu-CheckForUpdates");
    let shell = world
        .root
        .read_with(cx, |root, _| root.shell().expect("the shell").clone());
    assert!(shell.read_with(cx, |shell, _| shell.updates_dialog().is_some()));
}

#[gpui::test]
fn an_application_without_an_updater_says_so_and_draws_no_bar(cx: &mut TestAppContext) {
    let (world, cx) = start(cx, false);
    world.reach(cx);
    assert!(!world.on_screen(cx, "update-bar"));
    world.run(cx, CommandId::CheckForUpdates);
    assert!(world.on_screen(cx, "updates-unavailable"));
    assert!(world.on_screen(cx, "updates-primary"));
    assert!(!world.on_screen(cx, "updates-automatic"));
    // The one button closes it.
    world.click(cx, "updates-primary");
    let shell = world
        .root
        .read_with(cx, |root, _| root.shell().expect("the shell").clone());
    assert!(shell.read_with(cx, |shell, _| shell.updates_dialog().is_none()));
}
