//! Incoming webhooks in a channel's Details dialog on GPUI's test platform: who sees the section,
//! the list and its empty and failed states, adding with the address shown once, deleting with a
//! confirmation, and where the keyboard and focus go. The real views and the real core run; a
//! recording effect sink stands in for the runtime.
//!
//! Every keyboard claim is made with real key events through GPUI's dispatch. The test platform
//! never reports a window as active and gives every character the same width, so these prove
//! behavior and wiring; real layout is checked in the offscreen renders. Nothing here exercises a
//! real input method (beyond the composition guard), a screen reader, or a display.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, EntityInputHandler as _, Modifiers, TestAppContext, VisualTestContext,
    WindowAppearance,
};
use lazy_client_core::{
    ActionKind, ActionOutcome, ActionState, CreatedWebhook, Effect, Input, RequestError, SecretUrl,
    StartupState,
};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    Timestamp, UserId, Username, WebhookId, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    ApiError, BootstrapResponse, Conversation, ConversationUserState, ErrorCode, HistoryPage,
    PROTOCOL_VERSION, User, Webhook, Workspace, WorkspaceBootstrap,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::dialogs::IntegrationsSection;
use lazy_ui::root_view::RootView;

#[derive(Default)]
struct Recorder(RefCell<Vec<Effect>>);

impl EffectSink for Recorder {
    fn execute(&self, effects: Vec<Effect>) {
        self.0.borrow_mut().extend(effects);
    }
}

/// A made-up address. It stands for a secret: nothing here may leave the dialog's field.
const ADDRESS: &str = "http://127.0.0.1:8080/hooks/wh_test/s3cr3t-value-not-real";

struct World {
    model: Entity<AppModel>,
    effects: Rc<Recorder>,
    me: User,
    bob: User,
    workspace: WorkspaceId,
    /// A channel that is not the default one, so an admin may rename it and the dialog has every
    /// control.
    alerts: ConversationId,
    /// Selected at start.
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
    let (_root, cx) = cx.add_window_view({
        let model = model.clone();
        move |window, cx| RootView::new(model, window, cx)
    });
    let world = World {
        model,
        effects,
        me: person("alice"),
        bob: person("bob"),
        workspace: WorkspaceId::generate(),
        alerts: ConversationId::generate(),
        general: ConversationId::generate(),
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

fn only<T>(mut items: Vec<T>) -> T {
    assert_eq!(items.len(), 1, "exactly one expected");
    items.pop().unwrap()
}

impl World {
    fn channel(
        &self,
        id: ConversationId,
        name: &str,
        is_default: bool,
        archived: bool,
    ) -> Conversation {
        Conversation {
            id,
            workspace_id: self.workspace,
            kind: ConversationKind::Channel,
            name: Some(ChannelName::parse(name).unwrap()),
            topic: None,
            description: None,
            visibility: ChannelVisibility::Public,
            archived,
            is_default,
            created_by: None,
            participants: vec![],
            member_count: 2,
        }
    }

    fn bootstrap(&self, role: WorkspaceRole, archived: bool) -> BootstrapResponse {
        let state = |id| ConversationUserState {
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
                conversations: vec![
                    self.channel(self.general, "general", true, false),
                    self.channel(self.alerts, "alerts", false, archived),
                ],
                user_state: vec![state(self.general), state(self.alerts)],
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

    fn reach(&self, cx: &mut VisualTestContext, role: WorkspaceRole, archived: bool) {
        self.dispatch(
            cx,
            Input::Started(StartupState {
                session_restored: true,
                ..Default::default()
            }),
        );
        self.dispatch(
            cx,
            Input::BootstrapLoaded(Box::new(self.bootstrap(role, archived))),
        );
        let generation = self
            .take_effects()
            .into_iter()
            .find_map(|effect| match effect {
                Effect::FetchHistory { generation, .. } => Some(generation),
                _ => None,
            })
            .expect("loading the first channel fetches its history");
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

    fn as_admin(&self, cx: &mut VisualTestContext) {
        self.reach(cx, WorkspaceRole::Admin, false);
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

    /// Selects #alerts and opens its details the way a person does.
    fn open_details(&self, cx: &mut VisualTestContext) {
        self.click(cx, "conv-alerts");
        self.take_effects();
        self.click(cx, "details-button");
        assert!(self.on_screen(cx, "dialog-card"), "the dialog is open");
    }

    fn webhook(&self, name: &str) -> Webhook {
        Webhook {
            id: WebhookId::generate(),
            conversation_id: self.alerts,
            name: DisplayName::parse(name).unwrap(),
            bot_user_id: UserId::generate(),
            created_by: self.me.id,
            created_at: Timestamp::now(),
        }
    }

    /// The server's answer to listing.
    fn list_arrives(&self, cx: &mut VisualTestContext, list: Vec<Webhook>) {
        self.dispatch(
            cx,
            Input::ActionResult {
                kind: ActionKind::LoadWebhooks(self.alerts),
                result: Ok(ActionOutcome::Webhooks(list)),
            },
        );
        draw(cx);
    }

    /// Opens the details of #alerts with `list` already on the server.
    fn open_with(&self, cx: &mut VisualTestContext, list: Vec<Webhook>) {
        self.open_details(cx);
        self.list_arrives(cx, list);
    }

    fn state(&self, cx: &VisualTestContext, kind: ActionKind) -> ActionState {
        self.model
            .read_with(cx, |m, _| m.core().action(kind).clone())
    }

    fn held_address(&self, cx: &VisualTestContext) -> bool {
        self.model
            .read_with(cx, |m, _| m.core().created_webhook().is_some())
    }

    fn creates(&self) -> Vec<(ConversationId, String)> {
        self.take_effects()
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::CreateWebhook {
                    conversation,
                    request,
                } => Some((conversation, request.name.to_string())),
                _ => None,
            })
            .collect()
    }

    fn deletes(&self) -> Vec<WebhookId> {
        self.take_effects()
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::DeleteWebhook { webhook } => Some(webhook),
                _ => None,
            })
            .collect()
    }

    /// Moves focus from where the dialog starts (Topic) to Add incoming webhook: Name, then
    /// backwards around the ends through Save and Close.
    fn focus_add(&self, cx: &mut VisualTestContext) {
        cx.simulate_keystrokes("shift-tab shift-tab shift-tab shift-tab");
    }

    /// The server's answer to a creation: the webhook, and its address.
    fn created(&self, cx: &mut VisualTestContext, webhook: Webhook) {
        self.dispatch(
            cx,
            Input::ActionResult {
                kind: ActionKind::CreateWebhook(self.alerts),
                result: Ok(ActionOutcome::WebhookCreated(CreatedWebhook {
                    webhook,
                    url: SecretUrl::new(ADDRESS.to_owned()),
                })),
            },
        );
        draw(cx);
    }

    fn rejected(&self, code: ErrorCode, message: &str) -> Result<ActionOutcome, RequestError> {
        Err(RequestError::Rejected(ApiError::new(code, message)))
    }

    /// Adds "Deploy bot" through the keyboard and delivers the server's answer, leaving the
    /// address on screen.
    fn add_deploy_bot(&self, cx: &mut VisualTestContext) -> Webhook {
        self.focus_add(cx);
        cx.simulate_keystrokes("enter");
        cx.simulate_input("Deploy bot");
        cx.simulate_keystrokes("enter");
        let (conversation, name) = only(self.creates());
        assert_eq!((conversation, name.as_str()), (self.alerts, "Deploy bot"));
        let webhook = self.webhook("Deploy bot");
        self.created(cx, webhook.clone());
        webhook
    }
}

// ---- Who sees it ------------------------------------------------------------------------------------

#[gpui::test]
fn a_member_never_sees_integrations_or_loads_them(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, WorkspaceRole::Member, false);
    world.click(cx, "conv-alerts");
    world.take_effects();
    world.click(cx, "details-button");
    assert!(world.on_screen(cx, "dialog-card"));
    assert!(
        !world.on_screen(cx, "integrations") && !world.on_screen(cx, "webhook-add"),
        "members have no section"
    );
    assert!(
        !world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::LoadWebhooks { .. })),
        "and nothing is asked of the server for them"
    );
}

#[gpui::test]
fn an_admin_sees_loading_then_the_empty_state_with_a_way_to_add(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    world.open_details(cx);
    assert!(
        world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::LoadWebhooks { conversation } if *conversation == world.alerts)),
        "opening the section reads what is on the server"
    );
    assert!(world.on_screen(cx, "integrations-loading"));
    assert!(
        !world.on_screen(cx, "integrations-empty"),
        "an unanswered list is not an empty one"
    );

    world.list_arrives(cx, vec![]);
    assert!(!world.on_screen(cx, "integrations-loading"));
    assert!(world.on_screen(cx, "integrations-empty"));
    assert!(world.on_screen(cx, "webhook-add"));
    assert!(!world.on_screen(cx, "webhook-delete-0"));
}

#[gpui::test]
fn a_failed_list_says_so_and_retry_asks_again(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    world.open_details(cx);
    world.take_effects();
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::LoadWebhooks(world.alerts),
            result: Err(RequestError::Unknown("timed out".into())),
        },
    );
    draw(cx);
    assert!(
        world.on_screen(cx, "integrations-error"),
        "the failure is stated"
    );
    assert!(
        !world.on_screen(cx, "integrations-empty"),
        "a list that did not load is not claimed to be empty"
    );
    world.click(cx, "integrations-retry");
    let sent = world.take_effects();
    assert!(
        sent.iter()
            .any(|effect| matches!(effect, Effect::LoadWebhooks { .. })),
        "Retry asks again: {sent:?} {:?}",
        world.state(cx, ActionKind::LoadWebhooks(world.alerts))
    );
    world.list_arrives(cx, vec![world.webhook("Deploy bot")]);
    assert!(!world.on_screen(cx, "integrations-error"));
    assert!(world.on_screen(cx, "webhook-delete-0"));
}

#[gpui::test]
fn each_webhook_is_a_row_with_its_own_delete(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    world.open_with(
        cx,
        vec![world.webhook("Deploy bot"), world.webhook("Pager")],
    );
    assert!(world.on_screen(cx, "webhook-delete-0"));
    assert!(world.on_screen(cx, "webhook-delete-1"));
    assert!(!world.on_screen(cx, "webhook-delete-2"));
    assert!(!world.on_screen(cx, "integrations-empty"));
}

// ---- Adding -----------------------------------------------------------------------------------------

#[gpui::test]
fn adding_by_keyboard_sends_one_create_and_shows_the_address(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    world.open_with(cx, vec![]);

    world.focus_add(cx);
    cx.simulate_keystrokes("enter");
    assert!(
        world.on_screen(cx, "field-hint-Name of the app"),
        "Add opens the name field"
    );
    assert!(
        !world.on_screen(cx, "details-save") && !world.on_screen(cx, "details-archive"),
        "the rest of the dialog steps aside, so the form is never below the fold"
    );
    // The name field has the keyboard: typing goes straight into it.
    cx.simulate_input("  Deploy bot ");
    assert!(world.creates().is_empty(), "nothing is sent before Enter");
    cx.simulate_keystrokes("enter");
    let (conversation, name) = only(world.creates());
    assert_eq!(conversation, world.alerts);
    assert_eq!(
        name, "Deploy bot",
        "surrounding spaces are not part of the name"
    );

    // Enter again while it is in flight sends nothing more.
    cx.simulate_keystrokes("enter");
    assert!(world.creates().is_empty(), "one creation per answer");
    assert_eq!(
        world.state(cx, ActionKind::CreateWebhook(world.alerts)),
        ActionState::InFlight
    );

    world.created(cx, world.webhook("Deploy bot"));
    assert!(
        world.on_screen(cx, "webhook-address"),
        "the address is shown"
    );
    assert!(
        !world.on_screen(cx, "details-save") && !world.on_screen(cx, "details-leave"),
        "and it has the dialog to itself"
    );
    assert!(world.on_screen(cx, "webhook-note"));
    assert!(!world.on_screen(cx, "webhook-create"), "the form is gone");
    assert!(world.held_address(cx));
}

#[gpui::test]
fn an_empty_name_cannot_be_sent(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    world.open_with(cx, vec![]);
    world.click(cx, "webhook-add");
    cx.simulate_keystrokes("enter");
    cx.simulate_input("   ");
    cx.simulate_keystrokes("enter");
    world.click(cx, "webhook-create");
    assert!(world.creates().is_empty(), "blank names are not sent");
    assert!(world.on_screen(cx, "field-hint-Name of the app"));
}

#[gpui::test]
fn the_new_address_can_be_copied_with_the_button_and_with_the_keyboard(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    world.open_with(cx, vec![]);
    world.add_deploy_bot(cx);

    // Focus lands on Copy address, the first thing a person wants.
    assert!(clipboard(cx).is_none());
    cx.simulate_keystrokes("enter");
    assert_eq!(
        clipboard(cx).as_deref(),
        Some(ADDRESS),
        "Copy address copies it"
    );

    // The field itself is selectable and copyable though it cannot be edited.
    cx.simulate_keystrokes("shift-tab");
    cx.simulate_keystrokes("ctrl-a ctrl-c");
    assert_eq!(clipboard(cx).as_deref(), Some(ADDRESS));
    cx.simulate_input("typing changes nothing");
    cx.simulate_keystrokes("ctrl-a ctrl-c");
    assert_eq!(
        clipboard(cx).as_deref(),
        Some(ADDRESS),
        "the address cannot be edited"
    );
}

#[gpui::test]
fn the_address_is_forgotten_when_done_is_pressed(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    world.open_with(cx, vec![]);
    world.add_deploy_bot(cx);
    assert!(world.held_address(cx));

    world.click(cx, "webhook-done");
    assert!(
        !world.on_screen(cx, "webhook-address"),
        "the address is off the screen"
    );
    assert!(!world.held_address(cx), "and out of the client's memory");
    assert!(
        world.on_screen(cx, "webhook-add"),
        "the list is back, and the webhook is in it"
    );
    assert!(world.on_screen(cx, "webhook-delete-0"));
    assert!(world.on_screen(cx, "dialog-card"), "the dialog stays open");
}

#[gpui::test]
fn escape_forgets_the_address_and_steps_back_without_closing(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    world.open_with(cx, vec![]);
    world.add_deploy_bot(cx);

    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "webhook-address"));
    assert!(!world.held_address(cx));
    assert!(world.on_screen(cx, "dialog-card"), "one Escape, one step");
    cx.simulate_keystrokes("escape");
    assert!(
        !world.on_screen(cx, "dialog-card"),
        "the next closes the dialog"
    );
}

#[gpui::test]
fn closing_the_dialog_any_way_forgets_the_address(cx: &mut TestAppContext) {
    for how in ["close-button", "x", "scrim", "escape-twice"] {
        let (world, cx) = start(cx);
        world.as_admin(cx);
        world.open_with(cx, vec![]);
        world.add_deploy_bot(cx);
        assert!(world.held_address(cx), "{how}");
        match how {
            "close-button" => world.click(cx, "details-close"),
            "x" => world.click(cx, "dialog-close"),
            "scrim" => {
                draw(cx);
                cx.simulate_click(
                    gpui::point(gpui::px(2.), gpui::px(2.)),
                    Modifiers::default(),
                );
            }
            _ => cx.simulate_keystrokes("escape escape"),
        }
        cx.run_until_parked();
        assert!(!world.held_address(cx), "{how}: nothing is kept");
        if how == "close-button" {
            assert!(!world.on_screen(cx, "dialog-card"), "Close closes at once");
        }
    }
}

#[gpui::test]
fn the_address_is_not_there_when_the_dialog_is_opened_again(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    world.open_with(cx, vec![]);
    world.add_deploy_bot(cx);
    world.click(cx, "details-close");
    world.take_effects();

    world.click(cx, "details-button");
    assert!(world.on_screen(cx, "dialog-card"));
    assert!(!world.on_screen(cx, "webhook-address"));
    assert!(!world.on_screen(cx, "webhook-note"));
    assert!(
        world.on_screen(cx, "webhook-add"),
        "the list, as after any open"
    );
}

#[gpui::test]
fn a_refused_creation_is_explained_beside_the_name_and_can_be_retried(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    world.open_with(cx, vec![]);
    world.focus_add(cx);
    cx.simulate_keystrokes("enter");
    cx.simulate_input("Deploy bot");
    cx.simulate_keystrokes("enter");
    world.take_effects();

    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::CreateWebhook(world.alerts),
            result: world.rejected(ErrorCode::Conflict, "archived"),
        },
    );
    assert!(
        world.on_screen(cx, "field-error-Name of the app"),
        "the reason is right beside what was typed"
    );
    assert!(!world.held_address(cx));

    // The name is still there; Enter tries again.
    cx.simulate_keystrokes("enter");
    assert_eq!(only(world.creates()).1, "Deploy bot");
    world.created(cx, world.webhook("Deploy bot"));
    assert!(world.on_screen(cx, "webhook-address"));
    assert!(
        !world.on_screen(cx, "field-error-Name of the app"),
        "the error went with the form"
    );
}

#[gpui::test]
fn a_lost_answer_says_it_is_unknown_whether_it_was_created(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    world.open_with(cx, vec![]);
    world.focus_add(cx);
    cx.simulate_keystrokes("enter");
    cx.simulate_input("Deploy bot");
    cx.simulate_keystrokes("enter");
    world.take_effects();
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::CreateWebhook(world.alerts),
            result: Err(RequestError::Unknown("connection reset".into())),
        },
    );
    assert!(world.on_screen(cx, "field-error-Name of the app"));
    match world.state(cx, ActionKind::CreateWebhook(world.alerts)) {
        ActionState::Failed(message) => assert!(
            message.contains("isn't known whether the webhook was created"),
            "{message}"
        ),
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[gpui::test]
fn cancel_returns_to_the_list_and_discards_the_name(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    world.open_with(cx, vec![]);
    world.click(cx, "webhook-add");
    cx.simulate_keystrokes("enter");
    cx.simulate_input("Half typed");
    world.click(cx, "webhook-cancel");
    assert!(world.on_screen(cx, "webhook-add"));
    assert!(world.creates().is_empty());
    world.click(cx, "webhook-add");
    assert!(world.on_screen(cx, "webhook-create"));
    cx.simulate_keystrokes("enter");
    assert!(
        world.creates().is_empty(),
        "the old text was not kept, so Enter sends nothing"
    );
}

#[gpui::test]
fn escape_from_the_name_form_goes_back_to_the_list_first(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    world.open_with(cx, vec![]);
    world.click(cx, "webhook-add");
    assert!(world.on_screen(cx, "webhook-create"));
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "webhook-create"));
    assert!(world.on_screen(cx, "dialog-card"));
    assert!(world.on_screen(cx, "webhook-add"));
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "dialog-card"));
}

#[gpui::test]
fn tab_in_the_name_field_stays_inside_the_dialog(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    world.open_with(cx, vec![]);
    world.click(cx, "webhook-add");
    // Name -> Cancel -> Create -> Close -> Save -> (wraps) Name field of the dialog.
    for _ in 0..12 {
        cx.simulate_keystrokes("tab");
        assert!(
            world.on_screen(cx, "dialog-card"),
            "Tab never leaves the dialog"
        );
    }
    for _ in 0..12 {
        cx.simulate_keystrokes("shift-tab");
    }
    assert!(
        world.on_screen(cx, "webhook-create"),
        "and the form is still there"
    );
}

// ---- The limit and archived channels ----------------------------------------------------------------

#[gpui::test]
fn at_the_limit_add_is_unavailable_and_says_why(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    let full: Vec<Webhook> = (0..lazy_domain::MAX_WEBHOOKS_PER_CONVERSATION)
        .map(|n| world.webhook(&format!("App {n}")))
        .collect();
    world.open_with(cx, full);
    assert!(world.on_screen(cx, "integrations-full"));
    assert!(world.on_screen(cx, "webhook-delete-9"));
    world.click(cx, "webhook-add");
    assert!(
        !world.on_screen(cx, "webhook-create"),
        "a disabled Add does nothing"
    );
    // Deleting one makes room.
    world.click(cx, "webhook-delete-0");
    cx.simulate_keystrokes("tab enter");
    let gone = only(world.deletes());
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::DeleteWebhook(gone),
            result: Ok(ActionOutcome::Done),
        },
    );
    draw(cx);
    assert!(!world.on_screen(cx, "integrations-full"));
    world.click(cx, "webhook-add");
    assert!(world.on_screen(cx, "webhook-create"), "Add works again");
}

#[gpui::test]
fn the_keyboard_scrolls_the_list_to_the_row_it_reaches(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    let many: Vec<Webhook> = (0..lazy_domain::MAX_WEBHOOKS_PER_CONVERSATION)
        .map(|n| world.webhook(&format!("App {n}")))
        .collect();
    world.open_with(cx, many);
    let list = |cx: &mut VisualTestContext| {
        draw(cx);
        cx.debug_bounds("webhook-list")
            .expect("the list is on screen")
    };
    let inside = |cx: &mut VisualTestContext, row: &'static str| {
        let list = list(cx);
        let row = cx.debug_bounds(row).expect("the row is laid out");
        row.top() >= list.top() && row.bottom() <= list.bottom()
    };
    assert!(
        inside(cx, "webhook-delete-0"),
        "the first rows are in view when it opens"
    );
    assert!(
        !inside(cx, "webhook-delete-9"),
        "the last is below the few rows the list shows"
    );
    // Backwards from Topic: Name, Save, Close, Add incoming webhook, then the last Delete.
    cx.simulate_keystrokes("shift-tab shift-tab shift-tab shift-tab shift-tab");
    assert!(
        inside(cx, "webhook-delete-9"),
        "reaching a row with the keyboard brings it into view"
    );
}

#[gpui::test]
fn an_archived_channel_cannot_get_new_integrations(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, WorkspaceRole::Admin, true);
    world.open_with(cx, vec![world.webhook("Deploy bot")]);
    assert!(world.on_screen(cx, "integrations-archived"));
    world.click(cx, "webhook-add");
    assert!(!world.on_screen(cx, "webhook-create"));
    assert!(
        world.on_screen(cx, "webhook-delete-0"),
        "what exists can still be removed"
    );
}

// ---- Deleting ---------------------------------------------------------------------------------------

#[gpui::test]
fn delete_asks_first_and_starts_on_cancel(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    let (deploy, pager) = (world.webhook("Deploy bot"), world.webhook("Pager"));
    world.open_with(cx, vec![deploy.clone(), pager.clone()]);

    world.click(cx, "webhook-delete-1");
    assert!(
        world.on_screen(cx, "webhook-delete-confirm"),
        "it asks first"
    );
    assert!(world.deletes().is_empty(), "nothing is sent yet");

    // Focus is on Cancel: Enter backs out.
    cx.simulate_keystrokes("enter");
    assert!(!world.on_screen(cx, "webhook-delete-confirm"));
    assert!(
        world.on_screen(cx, "webhook-delete-1"),
        "both are still listed"
    );
    assert!(world.deletes().is_empty());

    // Escape backs out too.
    world.click(cx, "webhook-delete-1");
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "webhook-delete-confirm"));
    assert!(world.on_screen(cx, "dialog-card"));

    // Tab to the confirming button.
    world.click(cx, "webhook-delete-1");
    cx.simulate_keystrokes("tab enter");
    assert_eq!(
        world.deletes(),
        vec![pager.id],
        "the one that was asked about"
    );
    assert_eq!(
        world.state(cx, ActionKind::DeleteWebhook(pager.id)),
        ActionState::InFlight
    );
    // A second Enter while it is in flight sends nothing more.
    cx.simulate_keystrokes("enter");
    assert!(world.deletes().is_empty());

    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::DeleteWebhook(pager.id),
            result: Ok(ActionOutcome::Done),
        },
    );
    draw(cx);
    assert!(
        !world.on_screen(cx, "webhook-delete-confirm"),
        "back on the list"
    );
    assert!(
        world.on_screen(cx, "webhook-delete-0"),
        "Deploy bot remains"
    );
    assert!(!world.on_screen(cx, "webhook-delete-1"), "Pager is gone");
}

#[gpui::test]
fn a_refused_delete_is_explained_in_place_and_can_be_cancelled(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    let pager = world.webhook("Pager");
    world.open_with(cx, vec![pager.clone()]);
    world.click(cx, "webhook-delete-0");
    cx.simulate_keystrokes("tab enter");
    world.take_effects();
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::DeleteWebhook(pager.id),
            result: world.rejected(ErrorCode::Forbidden, "no"),
        },
    );
    draw(cx);
    assert!(
        world.on_screen(cx, "integrations-error"),
        "the reason is shown here"
    );
    assert!(
        world.on_screen(cx, "webhook-delete-confirm"),
        "and it can be tried again"
    );
    cx.simulate_keystrokes("escape");
    assert!(world.on_screen(cx, "webhook-delete-0"));
    assert!(
        !world.on_screen(cx, "integrations-error"),
        "stepping back clears the message"
    );
}

#[gpui::test]
fn an_address_on_screen_goes_when_its_webhook_is_deleted(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    world.open_with(cx, vec![]);
    let webhook = world.add_deploy_bot(cx);
    // Its webhook is deleted while the address is still showing.
    world.dispatch(cx, Input::DeleteWebhook(webhook.id));
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::DeleteWebhook(webhook.id),
            result: Ok(ActionOutcome::Done),
        },
    );
    draw(cx);
    assert!(!world.held_address(cx), "the core no longer holds it");
}

// ---- Leaving the channel while the section is open --------------------------------------------------

#[gpui::test]
fn losing_the_channel_closes_the_dialog_and_forgets_the_address(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.as_admin(cx);
    world.open_with(cx, vec![]);
    world.add_deploy_bot(cx);
    world.dispatch(cx, Input::LeaveChannel(world.alerts));
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::Leave(world.alerts),
            result: Ok(ActionOutcome::Done),
        },
    );
    draw(cx);
    assert!(!world.held_address(cx));
    assert!(!world.on_screen(cx, "webhook-address"));
}

// ---- Composition ------------------------------------------------------------------------------------

#[gpui::test]
fn escape_during_composition_in_the_name_belongs_to_the_composition(cx: &mut TestAppContext) {
    cx.update(|cx| lazy_ui::init(cx, WindowAppearance::Light));
    let effects = Rc::new(Recorder::default());
    let model = cx.new(|_| AppModel::new(effects));
    let conversation = ConversationId::generate();
    let (section, cx) = cx.add_window_view({
        let model = model.clone();
        move |window, cx| IntegrationsSection::new(model, conversation, window, cx)
    });
    let input = section.read_with(cx, |s, _| s.name_input().clone());
    // Enter the adding view, then start composing.
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    section.update_in(cx, |s, window, cx| s.start_adding(window, cx));
    input.update_in(cx, |input, window, cx| {
        input.replace_and_mark_text_in_range(None, "かな", None, window, cx)
    });
    cx.run_until_parked();
    assert!(input.read_with(cx, |i, _| i.is_composing()));
    let handled = section.update_in(cx, |s, window, cx| s.back(window, cx));
    assert!(handled, "Escape is swallowed so the composition can use it");
    assert!(
        input.read_with(cx, |i, _| i.is_composing()),
        "and the form is still showing its composition"
    );
}
