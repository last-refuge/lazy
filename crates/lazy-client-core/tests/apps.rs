//! Apps in the core (D35): who may manage them, checks before asking, the token held out of every
//! debug string until dismissed, and the list kept as the server answers.

mod common;

use common::{Driver, World};
use lazy_client_core::{ActionKind, ActionOutcome, ActionState, Change, Effect, Input, SecretUrl};
use lazy_domain::app::AppScope;
use lazy_domain::{AppId, UserId, WorkspaceRole};
use lazy_protocol::App;

const TOKEN: &str = "lzb_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd";

fn admin() -> (World, Driver) {
    let mut world = World::new();
    world.role = WorkspaceRole::Admin;
    let driver = Driver::ready(&world);
    (world, driver)
}

fn app(id: AppId, scopes: Vec<AppScope>) -> App {
    App {
        id,
        name: "Deployer".into(),
        bot_user_id: UserId::generate(),
        scopes,
        token_hint: "abcd".into(),
        channels: Vec::new(),
        created_by: UserId::generate(),
        created_at: lazy_domain::Timestamp::from(time::OffsetDateTime::UNIX_EPOCH),
        last_used_at: None,
        commands: Vec::new(),
        events: None,
        interactivity_url: None,
    }
}

fn app_effects(driver: &Driver) -> Vec<Effect> {
    driver
        .effects
        .iter()
        .filter(|e| {
            matches!(
                e,
                Effect::LoadApps { .. }
                    | Effect::CreateApp { .. }
                    | Effect::NewAppToken { .. }
                    | Effect::UpdateAppScopes { .. }
                    | Effect::SetAppChannel { .. }
                    | Effect::DeleteApp { .. }
            )
        })
        .cloned()
        .collect()
}

fn failed(driver: &Driver, kind: ActionKind) -> String {
    match driver.core.action(kind) {
        ActionState::Failed(text) => text.clone(),
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn members_are_not_offered_apps() {
    let mut world = World::new();
    world.role = WorkspaceRole::Member;
    let mut driver = Driver::ready(&world);
    let ws = world.workspace.id;
    assert!(!driver.core.can_manage_apps(ws));
    driver.send(Input::LoadApps(ws));
    driver.send(Input::CreateApp {
        workspace: ws,
        name: "x".into(),
        scopes: AppScope::DEFAULT.to_vec(),
    });
    assert!(app_effects(&driver).is_empty());
    assert!(failed(&driver, ActionKind::CreateApp(ws)).contains("admins"));
}

#[test]
fn making_an_app_checks_first_and_holds_the_token_out_of_sight() {
    let (world, mut driver) = admin();
    let ws = world.workspace.id;
    driver.send(Input::CreateApp {
        workspace: ws,
        name: "  ".into(),
        scopes: AppScope::DEFAULT.to_vec(),
    });
    assert!(failed(&driver, ActionKind::CreateApp(ws)).contains("name"));
    driver.send(Input::CreateApp {
        workspace: ws,
        name: "Deployer".into(),
        scopes: vec![AppScope::Unknown],
    });
    assert!(failed(&driver, ActionKind::CreateApp(ws)).contains("at least one"));
    assert!(app_effects(&driver).is_empty());

    driver.send(Input::CreateApp {
        workspace: ws,
        name: " Deployer ".into(),
        scopes: vec![AppScope::ChatWrite, AppScope::ChatWrite],
    });
    match app_effects(&driver).as_slice() {
        [Effect::CreateApp { request, .. }] => {
            assert_eq!(request.name, "Deployer");
            assert_eq!(request.scopes, [AppScope::ChatWrite]);
        }
        other => panic!("{other:?}"),
    }
    let id = AppId::generate();
    let out = driver.send(Input::ActionResult {
        kind: ActionKind::CreateApp(ws),
        result: Ok(ActionOutcome::AppToken(
            Box::new(app(id, vec![AppScope::ChatWrite])),
            SecretUrl::new(TOKEN.into()),
        )),
    });
    assert_eq!(
        driver.core.app_token(id).map(SecretUrl::as_str),
        Some(TOKEN)
    );
    assert_eq!(driver.core.apps(ws).map(<[App]>::len), Some(1));
    assert!(driver.changes.contains(&Change::AppTokenChanged));
    for text in [
        format!("{out:?}"),
        format!("{:?}", driver.core.app_token(id)),
    ] {
        assert!(!text.contains("0123456789abcdef"), "{text}");
    }
    driver.send(Input::DismissAppToken);
    assert!(driver.core.app_token(id).is_none());
}

#[test]
fn changes_replace_the_app_and_deleting_removes_it() {
    let (world, mut driver) = admin();
    let ws = world.workspace.id;
    let id = AppId::generate();
    driver.send(Input::LoadApps(ws));
    driver.send(Input::ActionResult {
        kind: ActionKind::LoadApps(ws),
        result: Ok(ActionOutcome::Apps(vec![app(
            id,
            vec![AppScope::ChatWrite],
        )])),
    });
    driver.send(Input::UpdateAppScopes {
        app: id,
        scopes: vec![],
    });
    assert!(failed(&driver, ActionKind::UpdateAppScopes(id)).contains("at least one"));
    driver.send(Input::UpdateAppScopes {
        app: id,
        scopes: vec![AppScope::ChannelsRead],
    });
    driver.send(Input::ActionResult {
        kind: ActionKind::UpdateAppScopes(id),
        result: Ok(ActionOutcome::App(Box::new(app(
            id,
            vec![AppScope::ChannelsRead],
        )))),
    });
    assert_eq!(
        driver.core.apps(ws).unwrap()[0].scopes,
        [AppScope::ChannelsRead]
    );
    driver.send(Input::DeleteApp(id));
    driver.send(Input::ActionResult {
        kind: ActionKind::DeleteApp(id),
        result: Ok(ActionOutcome::Done),
    });
    assert_eq!(driver.core.apps(ws).map(<[App]>::len), Some(0));
    driver.send(Input::SignOut);
    assert!(driver.core.apps(ws).is_none());
}

fn deploy_command() -> lazy_protocol::WorkspaceCommand {
    lazy_protocol::WorkspaceCommand {
        name: "deploy".into(),
        description: "Deploy a service".into(),
        usage_hint: "[service]".into(),
        app_name: "Deployer".into(),
    }
}

#[test]
fn a_known_command_runs_instead_of_sending_and_its_reply_is_private() {
    let mut world = World::new();
    world.role = WorkspaceRole::Member;
    let mut driver = Driver::ready(&world);
    let ws = world.workspace.id;
    let general = world.channel.id;

    // Typing a slash loads the workspace's commands.
    driver.send(Input::SetDraft {
        conversation: general,
        text: "/dep".into(),
    });
    assert!(
        driver
            .effects
            .iter()
            .any(|e| matches!(e, Effect::LoadCommands { workspace } if *workspace == ws))
    );
    driver.send(Input::ActionResult {
        kind: ActionKind::LoadCommands(ws),
        result: Ok(ActionOutcome::Commands(vec![deploy_command()])),
    });
    assert_eq!(driver.core.commands(ws).map(<[_]>::len), Some(1));

    // A command that is not one of them is an ordinary message.
    driver.effects.clear();
    driver.send(Input::SendMessage {
        conversation: general,
        text: "/shrug ok".into(),
    });
    assert!(
        !driver
            .effects
            .iter()
            .any(|e| matches!(e, Effect::RunCommand { .. }))
    );

    // A known one runs, and the draft is cleared; nothing is sent as a message.
    driver.effects.clear();
    driver.send(Input::SetDraft {
        conversation: general,
        text: "/deploy web prod".into(),
    });
    driver.send(Input::SendMessage {
        conversation: general,
        text: "/deploy web prod".into(),
    });
    match driver
        .effects
        .iter()
        .find(|e| matches!(e, Effect::RunCommand { .. }))
    {
        Some(Effect::RunCommand { request, .. }) => {
            assert_eq!(
                (request.command.as_str(), request.text.as_str()),
                ("deploy", "web prod")
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(driver.core.draft(general).is_empty());
    assert_eq!(driver.core.last_command(general), Some("/deploy web prod"));

    // Its private reply is held for the conversation until dismissed.
    let reply = lazy_domain::MessageDocument::from_markdown("Deploying *web*").unwrap();
    driver.send(Input::ActionResult {
        kind: ActionKind::RunCommand(general),
        result: Ok(ActionOutcome::CommandRan(Box::new(
            lazy_protocol::RunCommandResponse {
                app_name: "Deployer".into(),
                reply: Some(reply.clone()),
                posted: None,
            },
        ))),
    });
    let held = driver.core.command_reply(general).unwrap();
    assert_eq!(
        (held.app_name.as_str(), held.command.as_str()),
        ("Deployer", "/deploy web prod")
    );
    assert_eq!(held.document, reply);
    driver.send(Input::DismissCommandReply(general));
    assert!(driver.core.command_reply(general).is_none());
}

#[test]
fn a_button_press_waits_once_and_its_reply_shows_like_a_commands() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let general = world.channel.id;
    let message = lazy_domain::MessageId::generate();
    let press = |driver: &mut Driver, action: &str| {
        driver.send(Input::PressButton {
            conversation: general,
            message,
            block: 2,
            action_id: action.into(),
            label: "Approve".into(),
        })
    };
    press(&mut driver, "approve");
    let presses: Vec<_> = driver
        .effects
        .iter()
        .filter_map(|e| match e {
            Effect::PressButton {
                message: m,
                request,
                ..
            } => Some((*m, request.block, request.action_id.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(presses, [(message, 2, "approve".to_owned())]);
    assert_eq!(driver.core.pressing(general), Some((message, 2, "approve")));
    assert_eq!(driver.core.last_command(general), Some("Approve"));

    // A second press while the first waits is not sent.
    press(&mut driver, "reject");
    assert_eq!(
        driver
            .effects
            .iter()
            .filter(|e| matches!(e, Effect::PressButton { .. }))
            .count(),
        1
    );

    let reply = lazy_domain::MessageDocument::from_markdown("Approved").unwrap();
    driver.send(Input::ActionResult {
        kind: ActionKind::RunCommand(general),
        result: Ok(ActionOutcome::CommandRan(Box::new(
            lazy_protocol::RunCommandResponse {
                app_name: "Deployer".into(),
                reply: Some(reply.clone()),
                posted: None,
            },
        ))),
    });
    assert_eq!(driver.core.pressing(general), None);
    let held = driver.core.command_reply(general).unwrap();
    assert_eq!(
        (held.app_name.as_str(), held.command.as_str()),
        ("Deployer", "Approve")
    );
    assert_eq!(held.document, reply);

    // The next press is a new one, with its own id; a failure is the server's words.
    driver.effects.clear();
    press(&mut driver, "approve");
    assert!(driver.core.command_reply(general).is_none());
    driver.send(Input::ActionResult {
        kind: ActionKind::RunCommand(general),
        result: Err(lazy_client_core::RequestError::Rejected(
            lazy_protocol::ApiError::new(
                lazy_protocol::ErrorCode::Unavailable,
                "Deployer doesn't take button presses",
            ),
        )),
    });
    assert_eq!(
        failed(&driver, ActionKind::RunCommand(general)),
        "Deployer doesn't take button presses."
    );
}
