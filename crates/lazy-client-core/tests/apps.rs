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
