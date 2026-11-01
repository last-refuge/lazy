//! D24's rules for dialogs, checked for every dialog by one walk.
//!
//! For each dialog: opening puts the keyboard inside it; Tab and Shift+Tab cycle without leaving
//! it; Escape closes it and gives the keyboard back to what had it (here, the composer); Escape
//! while an input method has text in composition cancels the composition and does not close; and
//! Enter in a text field commits the composition before it submits anything.
//!
//! The list of dialogs is the `ShellDialog` enum, matched exhaustively below, so a new variant
//! does not compile until it is audited here; a new file in `src/dialogs` fails the file check
//! until it is accounted for. The emoji picker and the image viewer are not `ShellDialog`s and are
//! audited on their own at the end. A composition is simulated by the input handler's own calls,
//! not by a real input method.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, EntityInputHandler as _, Focusable as _, TestAppContext,
    VisualTestContext, WindowAppearance,
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
use lazy_ui::modal::{modal_has_focus, open_modal_count};
use lazy_ui::root_view::RootView;
use lazy_ui::shell::{PickerPurpose, ShellDialog, ShellView};
use lazy_ui::text_input::{TextInput, focused_input};

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
    design: ConversationId,
    message: MessageId,
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
        workspace: WorkspaceId::generate(),
        general: ConversationId::generate(),
        design: ConversationId::generate(),
        message: MessageId::generate(),
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

    /// The person is an owner, so every administrative dialog opens.
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
                role: WorkspaceRole::Owner,
                conversations: vec![
                    self.channel(self.general, "general"),
                    self.channel(self.design, "design"),
                ],
                user_state: vec![state(self.general), state(self.design)],
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
                        id: self.message,
                        conversation_id: self.general,
                        author_id: self.me.id,
                        document: MessageDocument::from_markdown("hello").unwrap(),
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

    fn shell(&self, cx: &mut VisualTestContext) -> Entity<ShellView> {
        self.root
            .read_with(cx, |root, _| root.shell().expect("the shell").clone())
    }

    fn composer_input(&self, cx: &mut VisualTestContext) -> Entity<TextInput> {
        let shell = self.shell(cx);
        shell.read_with(cx, |shell, cx| shell.composer().read(cx).input().clone())
    }

    /// Gives the keyboard to the composer, as it is after opening a conversation.
    fn focus_composer(&self, cx: &mut VisualTestContext) {
        let shell = self.shell(cx);
        shell.update_in(cx, |shell, window, cx| {
            shell
                .composer()
                .update(cx, |composer, cx| composer.focus_input(window, cx))
        });
        draw(cx);
    }

    fn open(&self, cx: &mut VisualTestContext, dialog: ShellDialog) {
        let shell = self.shell(cx);
        shell.update_in(cx, |shell, window, cx| {
            shell.open_dialog(dialog, window, cx)
        });
        draw(cx);
    }

    fn inside(&self, cx: &mut VisualTestContext) -> bool {
        cx.update(|window, cx| modal_has_focus(window, cx))
    }

    fn open_count(&self, cx: &mut VisualTestContext) -> usize {
        cx.update(|_, cx| open_modal_count(cx))
    }

    fn focused_id(&self, cx: &mut VisualTestContext) -> Option<gpui::FocusHandle> {
        cx.update(|window, cx| window.focused(cx))
    }

    fn typing_in(&self, cx: &mut VisualTestContext) -> Option<Entity<TextInput>> {
        cx.update(|window, cx| focused_input(window, cx))
    }
}

/// Every dialog, by the enum's own variants. The exhaustive match is the point: a variant added
/// to `ShellDialog` stops this compiling until it is given a place in the walk.
fn name_of(dialog: &ShellDialog) -> &'static str {
    match dialog {
        ShellDialog::CreateChannel => "create channel",
        ShellDialog::BrowseChannels => "browse channels",
        ShellDialog::NewMessage => "new message",
        ShellDialog::Groups => "user groups",
        ShellDialog::CustomEmoji => "custom emoji",
        ShellDialog::Preferences => "preferences",
        ShellDialog::Details(_) => "details",
        ShellDialog::AddPeople(_) => "add people",
        ShellDialog::QuickSwitcher => "quick switcher",
        ShellDialog::Profile(_) => "profile",
        ShellDialog::WorkspaceSettings => "workspace settings",
        ShellDialog::Invitations => "invitations",
        ShellDialog::Integrations => "integrations",
        ShellDialog::AddWorkspace => "add workspace",
        ShellDialog::DeleteMessage { .. } => "delete message",
        ShellDialog::Shortcuts => "shortcuts",
        ShellDialog::Diagnostics => "diagnostics",
        ShellDialog::Updates => "updates",
    }
}

fn every_dialog(world: &World) -> Vec<ShellDialog> {
    vec![
        ShellDialog::CreateChannel,
        ShellDialog::BrowseChannels,
        ShellDialog::NewMessage,
        ShellDialog::Groups,
        ShellDialog::CustomEmoji,
        ShellDialog::Preferences,
        ShellDialog::Details(world.general),
        ShellDialog::AddPeople(world.general),
        ShellDialog::QuickSwitcher,
        ShellDialog::Profile(world.bob.id),
        ShellDialog::WorkspaceSettings,
        ShellDialog::Invitations,
        ShellDialog::Integrations,
        ShellDialog::AddWorkspace,
        ShellDialog::DeleteMessage {
            conversation: world.general,
            message: world.message,
        },
        ShellDialog::Shortcuts,
        ShellDialog::Diagnostics,
        ShellDialog::Updates,
    ]
}

/// The source files in `src/dialogs`, each with what covers it here.
const DIALOG_FILES: &[(&str, &str)] = &[
    ("add_workspace", "ShellDialog::AddWorkspace"),
    ("browse", "ShellDialog::BrowseChannels"),
    ("create_channel", "ShellDialog::CreateChannel"),
    ("custom_emoji", "ShellDialog::CustomEmoji"),
    ("delete_message", "ShellDialog::DeleteMessage"),
    ("details", "ShellDialog::Details"),
    ("diagnostics", "ShellDialog::Diagnostics"),
    (
        "github_panel",
        "a view of ShellDialog::Integrations (GitHub; not a dialog of its own)",
    ),
    ("groups", "ShellDialog::Groups"),
    (
        "integrations",
        "a section of ShellDialog::Details (its webhook forms are not dialogs of their own)",
    ),
    ("invitations", "ShellDialog::Invitations"),
    (
        "people",
        "ShellDialog::NewMessage and ShellDialog::AddPeople",
    ),
    ("preferences", "ShellDialog::Preferences"),
    ("profile", "ShellDialog::Profile"),
    ("quick_switcher", "ShellDialog::QuickSwitcher"),
    ("shortcuts", "ShellDialog::Shortcuts"),
    ("updates", "ShellDialog::Updates"),
    ("workspace_integrations", "ShellDialog::Integrations"),
    ("workspace_settings", "ShellDialog::WorkspaceSettings"),
];

#[test]
fn every_dialog_file_is_accounted_for_in_the_audit() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/dialogs");
    let mut found: Vec<String> = std::fs::read_dir(&dir)
        .expect("the dialogs folder")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension()? == "rs").then(|| path.file_stem()?.to_str().map(str::to_owned))?
        })
        .filter(|stem| stem != "mod")
        .collect();
    found.sort();
    let listed: Vec<String> = DIALOG_FILES
        .iter()
        .map(|(stem, _)| (*stem).to_owned())
        .collect();
    assert_eq!(
        found, listed,
        "a dialog file was added or removed: give it a place in the audit (DIALOG_FILES, and \
         the walk below if it is a ShellDialog)"
    );
}

/// One dialog through every rule. Defects are collected, not asserted one by one, so a run lists
/// everything that is wrong.
fn audit(
    world: &World,
    cx: &mut VisualTestContext,
    dialog: ShellDialog,
    defects: &mut Vec<String>,
) {
    let name = name_of(&dialog);
    let mut fail = |text: String| defects.push(format!("{name}: {text}"));
    world.focus_composer(cx);
    let composer = world.composer_input(cx);
    let composer_focus = cx.update(|_, cx| composer.focus_handle(cx));
    world.take_effects();

    // --- Opening: the keyboard is inside it.
    world.open(cx, dialog.clone());
    if world.open_count(cx) != 1 {
        fail(format!(
            "opening it left {} dialogs open",
            world.open_count(cx)
        ));
        return;
    }
    if !world.inside(cx) {
        fail("the keyboard is not inside it when it opens".into());
        return;
    }

    // --- Tab and Shift+Tab cycle without leaving.
    let first = world.focused_id(cx);
    let mut field_stop_at = world.typing_in(cx).map(|_| 0usize);
    let mut cycled = false;
    for step in 1..=60usize {
        cx.simulate_keystrokes("tab");
        if !world.inside(cx) {
            fail(format!("Tab {step} left the dialog"));
            break;
        }
        if field_stop_at.is_none() && world.typing_in(cx).is_some() {
            field_stop_at = Some(step);
        }
        if world.focused_id(cx) == first {
            cycled = true;
            break;
        }
    }
    if !cycled {
        fail("Tab never came round to where it started within 60 presses".into());
    }
    let mut cycled_back = false;
    for step in 1..=60usize {
        cx.simulate_keystrokes("shift-tab");
        if !world.inside(cx) {
            fail(format!("Shift+Tab {step} left the dialog"));
            break;
        }
        if world.focused_id(cx) == first {
            cycled_back = true;
            break;
        }
    }
    if !cycled_back {
        fail("Shift+Tab never came round within 60 presses".into());
    }

    // --- Escape during a composition cancels it and does not close; Enter commits it.
    // Back on a text field, if the dialog has one.
    let mut field = world.typing_in(cx);
    if field.is_none() {
        for _ in 0..60 {
            cx.simulate_keystrokes("tab");
            field = world.typing_in(cx);
            if field.is_some() || world.focused_id(cx) == first {
                break;
            }
        }
    }
    if let Some(field) = field {
        field.update_in(cx, |input, window, cx| {
            input.replace_and_mark_text_in_range(None, "かな", None, window, cx)
        });
        if !field.read_with(cx, |input, _| input.is_composing()) {
            fail("the simulated composition did not start".into());
        } else {
            world.take_effects();
            cx.simulate_keystrokes("escape");
            if world.open_count(cx) != 1 {
                fail("Escape during composition closed the dialog".into());
                world.open(cx, dialog.clone());
                return;
            }
            if field.read_with(cx, |input, _| input.is_composing()) {
                fail("Escape during composition did not cancel the composition".into());
            }
            if field.read_with(cx, |input, _| input.text().contains("かな")) {
                fail("the cancelled composition's text stayed in the field".into());
            }
            // Enter commits the composition, and submits nothing. (Typing may ask for a search or a
            // page; that is reading, not submitting.)
            field.update_in(cx, |input, window, cx| {
                input.replace_and_mark_text_in_range(None, "かな", None, window, cx)
            });
            world.take_effects();
            cx.simulate_keystrokes("enter");
            if field.read_with(cx, |input, _| input.is_composing()) {
                fail("Enter did not end the composition".into());
            }
            if !field.read_with(cx, |input, _| input.text().contains("かな")) {
                fail("Enter did not commit the composition's text".into());
            }
            let submitted: Vec<String> = world
                .take_effects()
                .iter()
                .map(|effect| format!("{effect:?}"))
                .filter(|text| {
                    !["Load", "Fetch", "Persist", "Search", "Cancel"]
                        .iter()
                        .any(|read| text.starts_with(read))
                })
                .collect();
            if !submitted.is_empty() {
                fail(format!(
                    "Enter during composition submitted something: {submitted:?}"
                ));
            }
            if world.open_count(cx) != 1 {
                fail("Enter during composition closed the dialog".into());
                world.open(cx, dialog.clone());
                return;
            }
        }
    }

    // --- Escape closes it and the keyboard goes back to the composer.
    cx.simulate_keystrokes("escape");
    if world.open_count(cx) != 0 {
        fail("Escape did not close it".into());
        return;
    }
    let shell = world.shell(cx);
    if shell.read_with(cx, |shell, _| shell.has_overlay()) {
        fail("a dialog is still showing after Escape".into());
    }
    if world.focused_id(cx).as_ref() != Some(&composer_focus) {
        fail("the keyboard did not go back to the composer".into());
    }
}

#[gpui::test]
fn every_dialog_follows_the_rules(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    let mut defects = Vec::new();
    for dialog in every_dialog(&world) {
        audit(&world, cx, dialog, &mut defects);
        // Whatever a defective dialog left behind, the next starts clean.
        let shell = world.shell(cx);
        if shell.read_with(cx, |shell, _| shell.has_overlay()) {
            cx.simulate_keystrokes("escape escape");
        }
    }
    assert!(
        defects.is_empty(),
        "{} defect(s):\n  {}",
        defects.len(),
        defects.join("\n  ")
    );
}

#[gpui::test]
fn the_command_palette_and_the_menu_follow_the_same_rules(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.focus_composer(cx);
    // The palette is the quick switcher: Escape closes, Escape during composition does not.
    cx.simulate_keystrokes("ctrl-shift-p");
    assert!(world.inside(cx));
    let field = world
        .typing_in(cx)
        .expect("the palette's field has the keyboard");
    field.update_in(cx, |input, window, cx| {
        input.replace_and_mark_text_in_range(None, "pr", None, window, cx)
    });
    cx.simulate_keystrokes("escape");
    assert_eq!(
        world.open_count(cx),
        1,
        "Escape during composition keeps the palette open"
    );
    assert!(!field.read_with(cx, |input, _| input.is_composing()));
    cx.simulate_keystrokes("escape");
    assert_eq!(world.open_count(cx), 0);
    // The command menu gives the keyboard back to its button.
    world.focus_composer(cx);
    let shell = world.shell(cx);
    shell.update_in(cx, |shell, window, cx| shell.open_command_menu(window, cx));
    draw(cx);
    assert!(shell.read_with(cx, |shell, _| shell.command_menu().is_some()));
    cx.simulate_keystrokes("escape");
    assert!(shell.read_with(cx, |shell, _| shell.command_menu().is_none()));
}

// ----- What is not a ShellDialog -----

#[gpui::test]
fn the_emoji_picker_follows_the_rules(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.focus_composer(cx);
    let composer = world.composer_input(cx);
    let composer_focus = cx.update(|_, cx| composer.focus_handle(cx));
    let shell = world.shell(cx);
    shell.update_in(cx, |shell, window, cx| {
        shell.open_picker(
            PickerPurpose::Composer,
            gpui::point(gpui::px(600.), gpui::px(600.)),
            window,
            cx,
        )
    });
    draw(cx);
    assert!(
        shell.read_with(cx, |shell, _| shell.has_picker()),
        "the picker opened"
    );
    // The keyboard is in its search field.
    let search = world
        .typing_in(cx)
        .expect("the picker's search has the keyboard");
    // Tab keeps to the picker.
    let first = world.focused_id(cx);
    for step in 1..=60 {
        cx.simulate_keystrokes("tab");
        assert!(
            shell.read_with(cx, |shell, _| shell.has_picker()),
            "Tab {step} closed the picker"
        );
        let focused = world.focused_id(cx);
        assert!(focused.is_some(), "Tab {step} lost the keyboard");
        if focused == first {
            break;
        }
    }
    // Escape during a composition does not close it.
    cx.update(|window, cx| window.focus(&search.read(cx).focus_handle(cx), cx));
    search.update_in(cx, |input, window, cx| {
        input.replace_and_mark_text_in_range(None, "ta", None, window, cx)
    });
    cx.simulate_keystrokes("escape");
    assert!(
        shell.read_with(cx, |shell, _| shell.has_picker()),
        "Escape during composition keeps the picker open"
    );
    search.update_in(cx, |input, window, cx| input.unmark_text(window, cx));
    // Escape closes it and the keyboard goes back where it was.
    cx.simulate_keystrokes("escape");
    assert!(!shell.read_with(cx, |shell, _| shell.has_picker()));
    assert_eq!(world.focused_id(cx).as_ref(), Some(&composer_focus));
}

#[gpui::test]
fn the_image_viewer_follows_the_rules(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.focus_composer(cx);
    let composer = world.composer_input(cx);
    let composer_focus = cx.update(|_, cx| composer.focus_handle(cx));
    let picture = lazy_protocol::Attachment {
        id: lazy_domain::AttachmentId::generate(),
        name: "picture.png".into(),
        media_type: "image/png".into(),
        size: 1000,
        sha256: "0".repeat(64),
        width: Some(40),
        height: Some(30),
        has_thumbnail: false,
    };
    let shell = world.shell(cx);
    shell.update_in(cx, |shell, window, cx| {
        shell.open_viewer(vec![picture], 0, window, cx)
    });
    draw(cx);
    assert_eq!(world.open_count(cx), 1, "the viewer opened");
    assert!(world.inside(cx), "the keyboard is inside the viewer");
    let first = world.focused_id(cx);
    for step in 1..=30 {
        cx.simulate_keystrokes("tab");
        assert!(world.inside(cx), "Tab {step} left the viewer");
        if world.focused_id(cx) == first {
            break;
        }
    }
    for step in 1..=30 {
        cx.simulate_keystrokes("shift-tab");
        assert!(world.inside(cx), "Shift+Tab {step} left the viewer");
        if world.focused_id(cx) == first {
            break;
        }
    }
    cx.simulate_keystrokes("escape");
    assert_eq!(world.open_count(cx), 0, "Escape closed the viewer");
    assert_eq!(world.focused_id(cx).as_ref(), Some(&composer_focus));
}
