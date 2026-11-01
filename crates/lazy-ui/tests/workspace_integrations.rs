//! The workspace's Integrations dialog (D30) on GPUI's test platform: who may open it, the list
//! with each webhook's health, adding a webhook to a chosen channel with the address shown once,
//! a webhook's delivery log, a new address after a confirmation, and deleting. The real views and
//! the real core run; a recording effect sink stands in for the runtime.
//!
//! The test platform gives every character the same width and never shows a real display, so
//! these prove behavior and wiring; layout is checked in the offscreen renders.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, Focusable as _, Modifiers, TestAppContext, VisualTestContext,
    WindowAppearance,
};
use lazy_client_core::{
    ActionKind, ActionOutcome, ActionState, CreatedWebhook, Effect, Input, RequestError, SecretUrl,
    StartupState,
};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    MessageId, Timestamp, UserId, Username, WebhookId, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    BootstrapResponse, Conversation, ConversationUserState, HistoryPage, PROTOCOL_VERSION, User,
    Webhook, WebhookDelivery, WebhookDeliveryOutcome, Workspace, WorkspaceBootstrap,
    WorkspaceWebhook,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::dialogs::{IntegrationsDialog, IntegrationsView};
use lazy_ui::root_view::RootView;
use lazy_ui::shell::ShellDialog;

#[derive(Default)]
struct Recorder(RefCell<Vec<Effect>>);

impl EffectSink for Recorder {
    fn execute(&self, effects: Vec<Effect>) {
        self.0.borrow_mut().extend(effects);
    }
}

/// A made-up address. It stands for a secret.
const ADDRESS: &str = "http://127.0.0.1:8080/hooks/wh_test/s3cr3t-value-not-real";

struct World {
    root: Entity<RootView>,
    model: Entity<AppModel>,
    effects: Rc<Recorder>,
    me: User,
    workspace: WorkspaceId,
    general: ConversationId,
    alerts: ConversationId,
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
        root,
        model,
        effects,
        me: person("alice"),
        workspace: WorkspaceId::generate(),
        general: ConversationId::generate(),
        alerts: ConversationId::generate(),
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
    fn channel(&self, id: ConversationId, name: &str, is_default: bool) -> Conversation {
        Conversation {
            id,
            workspace_id: self.workspace,
            kind: ConversationKind::Channel,
            name: Some(ChannelName::parse(name).unwrap()),
            topic: None,
            description: None,
            visibility: ChannelVisibility::Public,
            archived: false,
            is_default,
            created_by: None,
            participants: vec![],
            member_count: 2,
        }
    }

    fn reach(&self, cx: &mut VisualTestContext, role: WorkspaceRole) {
        let state = |id| ConversationUserState {
            conversation_id: id,
            favorite: false,
            last_read: EventSeq::ZERO,
            unread_count: 0,
            mention_count: 0,
        };
        let bootstrap = BootstrapResponse {
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
                    self.channel(self.general, "general", true),
                    self.channel(self.alerts, "alerts", false),
                ],
                user_state: vec![state(self.general), state(self.alerts)],
                members: vec![self.me.clone()],
                head: EventSeq::ZERO,
            }],
        };
        self.dispatch(
            cx,
            Input::Started(StartupState {
                session_restored: true,
                ..Default::default()
            }),
        );
        self.dispatch(cx, Input::BootstrapLoaded(Box::new(bootstrap)));
        let generation = self
            .take_effects()
            .into_iter()
            .find_map(|effect| match effect {
                Effect::FetchHistory { generation, .. } => Some(generation),
                _ => None,
            })
            .expect("the first channel's history is fetched");
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

    fn dispatch(&self, cx: &mut VisualTestContext, input: Input) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.run_until_parked();
    }

    fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.effects.0.borrow_mut())
    }

    fn shell(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::shell::ShellView> {
        self.root
            .read_with(cx, |root, _| root.shell().expect("the shell").clone())
    }

    fn open(&self, cx: &mut VisualTestContext) {
        let shell = self.shell(cx);
        cx.update(|window, app| {
            shell.update(app, |shell, cx| {
                shell.open_dialog(ShellDialog::Integrations, window, cx)
            })
        });
        draw(cx);
    }

    fn dialog(&self, cx: &mut VisualTestContext) -> Option<Entity<IntegrationsDialog>> {
        let shell = self.shell(cx);
        shell.read_with(cx, |shell, _| shell.integrations_dialog())
    }

    fn view(&self, cx: &mut VisualTestContext) -> IntegrationsView {
        let dialog = self.dialog(cx).expect("the dialog is open");
        dialog.read_with(cx, |dialog, _| dialog.view())
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

    fn webhook(&self, name: &str, channel: ConversationId) -> Webhook {
        Webhook {
            id: WebhookId::generate(),
            conversation_id: channel,
            name: DisplayName::parse(name).unwrap(),
            bot_user_id: UserId::generate(),
            created_by: self.me.id,
            created_at: Timestamp::now(),
        }
    }

    fn entry(
        &self,
        webhook: &Webhook,
        channel: &str,
        last: Option<WebhookDeliveryOutcome>,
        failures: u32,
    ) -> WorkspaceWebhook {
        WorkspaceWebhook {
            webhook: webhook.clone(),
            channel_name: ChannelName::parse(channel).unwrap(),
            last_delivery: last.map(delivery),
            recent_failures: failures,
        }
    }

    fn list_arrives(&self, cx: &mut VisualTestContext, list: Vec<WorkspaceWebhook>) {
        self.dispatch(
            cx,
            Input::ActionResult {
                kind: ActionKind::LoadWorkspaceWebhooks(self.workspace),
                result: Ok(ActionOutcome::WorkspaceWebhooks(list)),
            },
        );
        draw(cx);
    }

    fn state(&self, cx: &VisualTestContext, kind: ActionKind) -> ActionState {
        self.model
            .read_with(cx, |m, _| m.core().action(kind).clone())
    }

    fn held_address(&self, cx: &VisualTestContext) -> bool {
        self.model
            .read_with(cx, |m, _| m.core().created_webhook().is_some())
    }
}

fn delivery(outcome: WebhookDeliveryOutcome) -> WebhookDelivery {
    WebhookDelivery {
        received_at: Timestamp::now(),
        outcome,
        message_id: outcome.posted().then(MessageId::generate),
        body_bytes: 42,
        idempotency_key: false,
    }
}

#[gpui::test]
fn a_member_cannot_open_integrations(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, WorkspaceRole::Member);
    world.open(cx);
    assert!(world.dialog(cx).is_none());
    assert!(
        world
            .take_effects()
            .iter()
            .all(|e| !matches!(e, Effect::LoadWorkspaceWebhooks { .. })),
        "nothing is asked"
    );
}

#[gpui::test]
fn the_list_loads_and_shows_each_webhook_with_its_health(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, WorkspaceRole::Admin);
    world.open(cx);
    assert!(world.take_effects().iter().any(
        |e| matches!(e, Effect::LoadWorkspaceWebhooks { workspace } if *workspace == world.workspace)
    ));
    assert!(world.on_screen(cx, "integrations-loading"));

    let healthy = world.webhook("Deploys", world.general);
    let failing = world.webhook("Alerts bot", world.alerts);
    world.list_arrives(
        cx,
        vec![
            world.entry(&healthy, "general", Some(WebhookDeliveryOutcome::Posted), 0),
            world.entry(&failing, "alerts", Some(WebhookDeliveryOutcome::Invalid), 3),
        ],
    );
    assert!(!world.on_screen(cx, "integrations-loading"));
    draw(cx);
    assert!(cx.debug_bounds("workspace-webhook-manage-0").is_some());
    assert!(cx.debug_bounds("workspace-webhook-manage-1").is_some());
    assert!(world.on_screen(cx, "workspace-webhook-health"));
}

#[gpui::test]
fn an_empty_workspace_says_so_and_offers_to_add(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, WorkspaceRole::Owner);
    world.open(cx);
    world.list_arrives(cx, vec![]);
    assert!(world.on_screen(cx, "integrations-empty"));
    assert!(world.on_screen(cx, "workspace-webhook-add"));
}

#[gpui::test]
fn a_failed_list_says_so_and_try_again_asks_again(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, WorkspaceRole::Admin);
    world.open(cx);
    world.take_effects();
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::LoadWorkspaceWebhooks(world.workspace),
            result: Err(RequestError::Unknown("timed out".into())),
        },
    );
    assert!(world.on_screen(cx, "integrations-error"));
    world.click(cx, "integrations-retry");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::LoadWorkspaceWebhooks { .. }))
    );
}

#[gpui::test]
fn adding_chooses_a_channel_and_shows_the_address_once(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, WorkspaceRole::Admin);
    world.open(cx);
    world.list_arrives(cx, vec![]);
    world.click(cx, "workspace-webhook-add");
    assert_eq!(world.view(cx), IntegrationsView::Adding);
    // Channels are offered by name: #alerts, then #general. The open channel (#general) is chosen
    // at first; the person picks #alerts.
    world.click(cx, "webhook-channel-0");
    // Choosing keeps the keyboard on the choice; the person moves to the name.
    let dialog = world.dialog(cx).unwrap();
    cx.update(|window, app| {
        let handle = dialog.read(app).name_input().focus_handle(app);
        window.focus(&handle, app);
    });
    cx.simulate_input("Pager");
    cx.simulate_keystrokes("enter");
    let creates: Vec<_> = world
        .take_effects()
        .into_iter()
        .filter_map(|e| match e {
            Effect::CreateWebhook {
                conversation,
                request,
            } => Some((conversation, request.name.to_string())),
            _ => None,
        })
        .collect();
    assert_eq!(creates, vec![(world.alerts, "Pager".to_owned())]);

    let webhook = world.webhook("Pager", world.alerts);
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::CreateWebhook(world.alerts),
            result: Ok(ActionOutcome::WebhookCreated(CreatedWebhook {
                webhook,
                url: SecretUrl::new(ADDRESS.to_owned()),
            })),
        },
    );
    assert_eq!(world.view(cx), IntegrationsView::Address { back: None });
    assert!(world.on_screen(cx, "webhook-address"));
    assert!(world.on_screen(cx, "webhook-usage"));
    // The list is asked again so the new webhook appears in it.
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::LoadWorkspaceWebhooks { .. }))
    );
    world.click(cx, "webhook-copy");
    assert_eq!(clipboard(cx).as_deref(), Some(ADDRESS));
    world.click(cx, "webhook-done");
    assert_eq!(world.view(cx), IntegrationsView::List);
    assert!(!world.held_address(cx), "the address is forgotten");
    let shown = world.dialog(cx).unwrap().read_with(cx, |dialog, cx| {
        dialog.address_input().read(cx).text().to_owned()
    });
    assert!(shown.is_empty(), "and the field is emptied");
}

#[gpui::test]
fn details_show_the_delivery_log_with_words_for_each_outcome(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, WorkspaceRole::Admin);
    world.open(cx);
    let hook = world.webhook("Deploys", world.general);
    world.list_arrives(cx, vec![world.entry(&hook, "general", None, 0)]);
    world.take_effects();
    world.click(cx, "workspace-webhook-manage-0");
    assert_eq!(world.view(cx), IntegrationsView::Details(hook.id));
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::LoadWebhookDeliveries { webhook } if *webhook == hook.id))
    );
    assert!(world.on_screen(cx, "webhook-log-loading"));
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::LoadWebhookDeliveries(hook.id),
            result: Ok(ActionOutcome::WebhookDeliveries(vec![
                delivery(WebhookDeliveryOutcome::RateLimited),
                delivery(WebhookDeliveryOutcome::Invalid),
                delivery(WebhookDeliveryOutcome::Posted),
            ])),
        },
    );
    draw(cx);
    assert!(world.on_screen(cx, "webhook-delivery-log"));
    assert!(
        world.on_screen(cx, "webhook-log-retry"),
        "it says why there is no resend"
    );
    // Refresh asks again; Escape goes back to the list.
    world.click(cx, "webhook-details-refresh");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::LoadWebhookDeliveries { .. }))
    );
    cx.simulate_keystrokes("escape");
    assert_eq!(world.view(cx), IntegrationsView::List);
    assert!(world.dialog(cx).is_some(), "Escape stepped back, not out");
}

#[gpui::test]
fn a_new_address_is_confirmed_first_then_shown_once(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, WorkspaceRole::Admin);
    world.open(cx);
    let hook = world.webhook("Deploys", world.general);
    world.list_arrives(cx, vec![world.entry(&hook, "general", None, 0)]);
    world.click(cx, "workspace-webhook-manage-0");
    world.take_effects();
    world.click(cx, "webhook-new-address");
    assert_eq!(world.view(cx), IntegrationsView::ConfirmNewAddress(hook.id));
    assert!(
        world
            .take_effects()
            .iter()
            .all(|e| !matches!(e, Effect::NewWebhookAddress { .. })),
        "nothing changes before the person says yes"
    );
    // Cancel has the keyboard first; Escape is the same as Cancel.
    cx.simulate_keystrokes("escape");
    assert_eq!(world.view(cx), IntegrationsView::Details(hook.id));

    world.click(cx, "webhook-new-address");
    world.click(cx, "webhook-confirm");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::NewWebhookAddress { webhook } if *webhook == hook.id))
    );
    assert_eq!(
        world.state(cx, ActionKind::NewWebhookAddress(hook.id)),
        ActionState::InFlight
    );
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::NewWebhookAddress(hook.id),
            result: Ok(ActionOutcome::WebhookCreated(CreatedWebhook {
                webhook: hook.clone(),
                url: SecretUrl::new(ADDRESS.to_owned()),
            })),
        },
    );
    assert_eq!(
        world.view(cx),
        IntegrationsView::Address {
            back: Some(hook.id)
        }
    );
    // Done returns to the webhook's details, and the address is gone.
    world.click(cx, "webhook-done");
    assert_eq!(world.view(cx), IntegrationsView::Details(hook.id));
    assert!(!world.held_address(cx));
}

#[gpui::test]
fn deleting_is_confirmed_and_returns_to_the_list(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, WorkspaceRole::Admin);
    world.open(cx);
    let hook = world.webhook("Deploys", world.general);
    world.list_arrives(cx, vec![world.entry(&hook, "general", None, 0)]);
    world.click(cx, "workspace-webhook-manage-0");
    world.click(cx, "webhook-delete");
    assert_eq!(world.view(cx), IntegrationsView::ConfirmDelete(hook.id));
    world.take_effects();
    world.click(cx, "webhook-confirm");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::DeleteWebhook { webhook } if *webhook == hook.id))
    );
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::DeleteWebhook(hook.id),
            result: Ok(ActionOutcome::Done),
        },
    );
    assert_eq!(world.view(cx), IntegrationsView::List);
    assert!(world.on_screen(cx, "integrations-empty"));
}

#[gpui::test]
fn closing_from_the_address_view_forgets_the_address(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, WorkspaceRole::Admin);
    world.open(cx);
    let hook = world.webhook("Deploys", world.general);
    world.list_arrives(cx, vec![world.entry(&hook, "general", None, 0)]);
    world.click(cx, "workspace-webhook-manage-0");
    world.click(cx, "webhook-new-address");
    world.click(cx, "webhook-confirm");
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::NewWebhookAddress(hook.id),
            result: Ok(ActionOutcome::WebhookCreated(CreatedWebhook {
                webhook: hook.clone(),
                url: SecretUrl::new(ADDRESS.to_owned()),
            })),
        },
    );
    assert!(world.held_address(cx));
    world.click(cx, "dialog-close");
    assert!(world.dialog(cx).is_none());
    assert!(!world.held_address(cx));
}

#[gpui::test]
fn losing_the_admin_role_closes_the_dialog(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, WorkspaceRole::Admin);
    world.open(cx);
    world.list_arrives(cx, vec![]);
    assert!(world.dialog(cx).is_some());
    world.reach(cx, WorkspaceRole::Member);
    assert!(world.dialog(cx).is_none());
}

// ----- GitHub (D32) -----

fn github_status(
    world: &World,
    connected: bool,
    installed: bool,
) -> lazy_protocol::GithubStatusResponse {
    lazy_protocol::GithubStatusResponse {
        available: true,
        connection: connected.then(|| lazy_protocol::GithubConnection {
            app_slug: "lazy-test".into(),
            html_url: "https://github.com/apps/lazy-test".into(),
            owner: "last-refuge".into(),
            bot_user_id: UserId::generate(),
            created_by: world.me.id,
            created_at: Timestamp::now(),
        }),
        installations: if installed {
            vec![lazy_protocol::GithubInstallation {
                account: "last-refuge".into(),
                all_repositories: true,
                suspended: false,
            }]
        } else {
            Vec::new()
        },
        subscriptions: Vec::new(),
        deliveries: if installed {
            vec![lazy_protocol::GithubDelivery {
                id: 41,
                event: "issues".into(),
                action: Some("opened".into()),
                repository: Some("last-refuge/lazy".into()),
                state: lazy_protocol::GithubDeliveryState::Failed,
                attempts: 6,
                last_error: Some("the database failed".into()),
                received_at: Timestamp::now(),
            }]
        } else {
            Vec::new()
        },
    }
}

impl World {
    fn github_arrives(
        &self,
        cx: &mut VisualTestContext,
        status: lazy_protocol::GithubStatusResponse,
    ) {
        self.dispatch(
            cx,
            Input::ActionResult {
                kind: ActionKind::LoadGithub(self.workspace),
                result: Ok(ActionOutcome::Github(Box::new(status))),
            },
        );
        draw(cx);
    }

    fn open_github(&self, cx: &mut VisualTestContext, status: lazy_protocol::GithubStatusResponse) {
        self.open(cx);
        self.list_arrives(cx, vec![]);
        self.github_arrives(cx, status.clone());
        self.click(cx, "github-open-panel");
        assert_eq!(self.view(cx), IntegrationsView::Github);
        self.github_arrives(cx, status);
    }
}

#[gpui::test]
fn the_github_card_says_where_things_stand_and_opens_the_github_view(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, WorkspaceRole::Admin);
    world.open(cx);
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::LoadGithub { .. })),
        "opening Integrations reads GitHub's status"
    );
    world.github_arrives(cx, github_status(&world, false, false));
    assert!(world.on_screen(cx, "github-card-status"));
    world.click(cx, "github-open-panel");
    assert_eq!(world.view(cx), IntegrationsView::Github);
    assert!(world.on_screen(cx, "github-panel"));
    // Escape from the GitHub overview returns to the list, not out of the dialog.
    cx.simulate_keystrokes("escape");
    assert_eq!(world.view(cx), IntegrationsView::List);
    assert!(world.dialog(cx).is_some());
}

#[gpui::test]
fn connecting_opens_the_browser_with_the_setup_address_and_forgets_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, WorkspaceRole::Admin);
    world.open_github(cx, github_status(&world, false, false));
    world.take_effects();
    let panel = world
        .dialog(cx)
        .unwrap()
        .read_with(cx, |d, _| d.github_panel())
        .unwrap();
    cx.update(|window, app| {
        let handle = panel.read(app).organization_input().focus_handle(app);
        window.focus(&handle, app);
    });
    cx.simulate_input("last-refuge");
    cx.simulate_keystrokes("enter");
    let asked: Vec<String> = world
        .take_effects()
        .into_iter()
        .filter_map(|e| match e {
            Effect::StartGithubSetup { request, .. } => Some(request.organization),
            _ => None,
        })
        .collect();
    assert_eq!(asked, vec!["last-refuge".to_owned()]);
    let url = "https://chat.example/github/setup/0011-secret";
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::StartGithubSetup(world.workspace),
            result: Ok(ActionOutcome::GithubSetup(SecretUrl::new(url.to_owned()))),
        },
    );
    draw(cx);
    assert_eq!(
        cx.opened_url().as_deref(),
        Some(url),
        "the browser takes over"
    );
    assert!(
        world
            .model
            .read_with(cx, |m, _| m.core().github_setup_url().is_none()),
        "and the address is forgotten"
    );
    assert!(
        world.on_screen(cx, "github-continue"),
        "it says what to do in the browser"
    );
}

#[gpui::test]
fn installed_github_follows_a_repository_and_retries_a_failed_delivery(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, WorkspaceRole::Admin);
    world.open_github(cx, github_status(&world, true, true));
    assert!(world.on_screen(cx, "github-connected"));
    assert!(world.on_screen(cx, "github-no-subscriptions"));
    world.take_effects();

    // Retry on the failed delivery.
    world.click(cx, "github-retry-0");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::RetryGithubDelivery { delivery } if *delivery == 41))
    );

    // Add a repository: the open channel (#general) is chosen; type and Follow.
    world.click(cx, "github-add");
    let panel = world
        .dialog(cx)
        .unwrap()
        .read_with(cx, |d, _| d.github_panel())
        .unwrap();
    assert_eq!(
        panel.read_with(cx, |p, _| p.view()),
        lazy_ui::dialogs::GithubView::Adding
    );
    cx.simulate_input("last-refuge/lazy");
    // Turn Comments on.
    world.click(cx, "github-event-comments");
    world.click(cx, "github-follow");
    let follows: Vec<_> = world
        .take_effects()
        .into_iter()
        .filter_map(|e| match e {
            Effect::GithubSubscribe {
                conversation,
                request,
            } => Some((conversation, request.repository, request.events)),
            _ => None,
        })
        .collect();
    assert_eq!(follows.len(), 1);
    assert_eq!(follows[0].0, world.general);
    assert_eq!(follows[0].1, "last-refuge/lazy");
    assert!(
        follows[0]
            .2
            .contains(&lazy_domain::github::GithubEvent::Comments)
    );
    assert!(
        follows[0]
            .2
            .contains(&lazy_domain::github::GithubEvent::PullRequest)
    );
    world.dispatch(
        cx,
        Input::ActionResult {
            kind: ActionKind::GithubSubscribe(world.general),
            result: Ok(ActionOutcome::Done),
        },
    );
    assert_eq!(
        panel.read_with(cx, |p, _| p.view()),
        lazy_ui::dialogs::GithubView::Overview,
        "back to the overview, which reloads"
    );
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::LoadGithub { .. }))
    );
}

#[gpui::test]
fn disconnecting_is_confirmed_first(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, WorkspaceRole::Admin);
    world.open_github(cx, github_status(&world, true, false));
    assert!(
        world.on_screen(cx, "github-install"),
        "not installed: Install on GitHub"
    );
    world.take_effects();
    world.click(cx, "github-disconnect");
    assert!(world.on_screen(cx, "github-disconnect-question"));
    assert!(
        world
            .take_effects()
            .iter()
            .all(|e| !matches!(e, Effect::DisconnectGithub { .. })),
        "nothing happens before yes"
    );
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "github-disconnect-question"));
    world.click(cx, "github-disconnect");
    world.click(cx, "github-disconnect-confirm");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::DisconnectGithub { .. }))
    );
}
