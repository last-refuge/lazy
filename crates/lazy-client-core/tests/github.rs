//! The GitHub connector in the core (D32): who may manage it, the status, the one-time setup
//! address kept out of every debug string, subscriptions checked before asking, and reloading
//! after each change.

mod common;

use common::{Driver, World};
use lazy_client_core::{ActionKind, ActionOutcome, ActionState, Change, Effect, Input, SecretUrl};
use lazy_domain::github::GithubEvent;
use lazy_domain::{GithubSubscriptionId, WorkspaceRole};
use lazy_protocol::GithubStatusResponse;

const SETUP: &str = "https://chat.example/github/setup/0123456789abcdef-secret-state";

fn admin() -> (World, Driver) {
    let mut world = World::new();
    world.role = WorkspaceRole::Admin;
    let driver = Driver::ready(&world);
    (world, driver)
}

fn github_effects(driver: &Driver) -> Vec<Effect> {
    driver
        .effects
        .iter()
        .filter(|e| {
            matches!(
                e,
                Effect::LoadGithub { .. }
                    | Effect::StartGithubSetup { .. }
                    | Effect::GithubSubscribe { .. }
                    | Effect::GithubUnsubscribe { .. }
                    | Effect::RetryGithubDelivery { .. }
                    | Effect::DisconnectGithub { .. }
            )
        })
        .cloned()
        .collect()
}

fn empty_status() -> GithubStatusResponse {
    GithubStatusResponse {
        available: true,
        connection: None,
        installations: Vec::new(),
        subscriptions: Vec::new(),
        deliveries: Vec::new(),
    }
}

fn failed(driver: &Driver, kind: ActionKind) -> String {
    match driver.core.action(kind) {
        ActionState::Failed(text) => text.clone(),
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn members_are_not_offered_github() {
    let mut world = World::new();
    world.role = WorkspaceRole::Member;
    let mut driver = Driver::ready(&world);
    let ws = world.workspace.id;
    assert!(!driver.core.can_manage_github(ws));
    driver.send(Input::LoadGithub(ws));
    driver.send(Input::StartGithubSetup {
        workspace: ws,
        organization: "last-refuge".into(),
    });
    driver.send(Input::DisconnectGithub(ws));
    assert!(github_effects(&driver).is_empty(), "nothing is asked");
    assert!(driver.core.github(ws).is_none());
}

#[test]
fn the_status_is_loaded_held_and_forgotten_at_sign_out() {
    let (world, mut driver) = admin();
    let ws = world.workspace.id;
    driver.send(Input::LoadGithub(ws));
    assert!(matches!(
        github_effects(&driver).as_slice(),
        [Effect::LoadGithub { workspace }] if *workspace == ws
    ));
    driver.send(Input::ActionResult {
        kind: ActionKind::LoadGithub(ws),
        result: Ok(ActionOutcome::Github(Box::new(empty_status()))),
    });
    assert!(
        driver
            .changes
            .contains(&Change::GithubChanged { workspace: ws })
    );
    assert!(driver.core.github(ws).is_some_and(|s| s.available));
    driver.send(Input::SignOut);
    assert!(driver.core.github(ws).is_none());
}

#[test]
fn connecting_checks_the_organization_and_holds_the_address_out_of_sight() {
    let (world, mut driver) = admin();
    let ws = world.workspace.id;
    driver.send(Input::StartGithubSetup {
        workspace: ws,
        organization: "not an org".into(),
    });
    assert!(github_effects(&driver).is_empty());
    assert!(failed(&driver, ActionKind::StartGithubSetup(ws)).contains("organization"));

    driver.send(Input::StartGithubSetup {
        workspace: ws,
        organization: " last-refuge ".into(),
    });
    assert!(matches!(
        github_effects(&driver).as_slice(),
        [Effect::StartGithubSetup { request, .. }] if request.organization == "last-refuge"
    ));
    let out = driver.send(Input::ActionResult {
        kind: ActionKind::StartGithubSetup(ws),
        result: Ok(ActionOutcome::GithubSetup(SecretUrl::new(SETUP.to_owned()))),
    });
    assert_eq!(
        driver.core.github_setup_url().map(SecretUrl::as_str),
        Some(SETUP)
    );
    for text in [
        format!("{out:?}"),
        format!("{:?}", driver.core.github_setup_url()),
    ] {
        assert!(!text.contains("secret-state"), "{text}");
    }
    driver.send(Input::DismissGithubSetup);
    assert!(driver.core.github_setup_url().is_none());
}

#[test]
fn a_subscription_is_checked_before_asking_and_the_status_reloads_after_it() {
    let (world, mut driver) = admin();
    let ws = world.workspace.id;
    let general = world.channel.id;
    driver.send(Input::GithubSubscribe {
        conversation: general,
        repository: "lazy".into(),
        events: GithubEvent::DEFAULT.to_vec(),
    });
    assert!(failed(&driver, ActionKind::GithubSubscribe(general)).contains("owner/name"));
    driver.send(Input::GithubSubscribe {
        conversation: general,
        repository: "last-refuge/lazy".into(),
        events: vec![],
    });
    assert!(failed(&driver, ActionKind::GithubSubscribe(general)).contains("at least one"));
    assert!(github_effects(&driver).is_empty());

    driver.send(Input::GithubSubscribe {
        conversation: general,
        repository: "https://github.com/Last-Refuge/Lazy".into(),
        events: vec![GithubEvent::Push],
    });
    assert!(matches!(
        github_effects(&driver).as_slice(),
        [Effect::GithubSubscribe { request, .. }] if request.repository == "last-refuge/lazy"
    ));
    driver.effects.clear();
    driver.send(Input::ActionResult {
        kind: ActionKind::GithubSubscribe(general),
        result: Ok(ActionOutcome::Done),
    });
    assert!(matches!(
        github_effects(&driver).as_slice(),
        [Effect::LoadGithub { workspace }] if *workspace == ws
    ));
}

#[test]
fn removing_retrying_and_disconnecting_reload_the_status() {
    let (world, mut driver) = admin();
    let ws = world.workspace.id;
    driver.send(Input::LoadGithub(ws));
    driver.send(Input::ActionResult {
        kind: ActionKind::LoadGithub(ws),
        result: Ok(ActionOutcome::Github(Box::new(empty_status()))),
    });
    let sub = GithubSubscriptionId::generate();
    for (input, kind) in [
        (
            Input::GithubUnsubscribe(sub),
            ActionKind::GithubUnsubscribe(sub),
        ),
        (
            Input::RetryGithubDelivery {
                workspace: ws,
                delivery: 7,
            },
            ActionKind::RetryGithubDelivery(7),
        ),
        (
            Input::DisconnectGithub(ws),
            ActionKind::DisconnectGithub(ws),
        ),
    ] {
        driver.effects.clear();
        driver.send(input);
        assert_eq!(github_effects(&driver).len(), 1, "{kind:?} asks");
        driver.effects.clear();
        driver.send(Input::ActionResult {
            kind,
            result: Ok(ActionOutcome::Done),
        });
        assert!(
            github_effects(&driver)
                .iter()
                .any(|e| matches!(e, Effect::LoadGithub { .. })),
            "{kind:?} reloads"
        );
        driver.send(Input::ActionResult {
            kind: ActionKind::LoadGithub(ws),
            result: Ok(ActionOutcome::Github(Box::new(empty_status()))),
        });
    }
}
