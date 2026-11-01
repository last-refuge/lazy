//! Webhook connectors in the core (D34): who may manage them, statuses per provider, a pasted
//! secret and a made one both kept out of every debug string, subscriptions checked before asking,
//! and reloading after each change.

mod common;

use common::{Driver, World};
use lazy_client_core::{ActionKind, ActionOutcome, ActionState, Change, Effect, Input, SecretUrl};
use lazy_domain::WorkspaceRole;
use lazy_domain::connector::ConnectorProvider::{self, Jira, Linear};
use lazy_protocol::ConnectorStatusResponse;

const PASTED: &str = "lin_wh_pasted-9c3e1f";
const MADE: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";

fn admin() -> (World, Driver) {
    let mut world = World::new();
    world.role = WorkspaceRole::Admin;
    let driver = Driver::ready(&world);
    (world, driver)
}

fn connector_effects(driver: &Driver) -> Vec<Effect> {
    driver
        .effects
        .iter()
        .filter(|e| {
            matches!(
                e,
                Effect::LoadConnector { .. }
                    | Effect::ConnectConnector { .. }
                    | Effect::SetConnectorSecret { .. }
                    | Effect::GenerateConnectorSecret { .. }
                    | Effect::ConnectorSubscribe { .. }
                    | Effect::ConnectorUnsubscribe { .. }
                    | Effect::RetryConnectorDelivery { .. }
                    | Effect::DisconnectConnector { .. }
            )
        })
        .cloned()
        .collect()
}

fn empty_status(provider: ConnectorProvider) -> ConnectorStatusResponse {
    ConnectorStatusResponse {
        provider,
        available: true,
        connection: None,
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
fn members_are_not_offered_connectors() {
    let mut world = World::new();
    world.role = WorkspaceRole::Member;
    let mut driver = Driver::ready(&world);
    let ws = world.workspace.id;
    assert!(!driver.core.can_manage_connectors(ws));
    driver.send(Input::LoadConnector {
        workspace: ws,
        provider: Linear,
    });
    driver.send(Input::GenerateConnectorSecret {
        workspace: ws,
        provider: Jira,
    });
    driver.send(Input::DisconnectConnector {
        workspace: ws,
        provider: Linear,
    });
    assert!(connector_effects(&driver).is_empty(), "nothing is asked");
    assert!(driver.core.connector(ws, Linear).is_none());
}

#[test]
fn each_provider_has_its_own_status_and_all_are_forgotten_at_sign_out() {
    let (world, mut driver) = admin();
    let ws = world.workspace.id;
    driver.send(Input::ConnectConnector {
        workspace: ws,
        provider: Linear,
    });
    assert!(matches!(
        connector_effects(&driver).as_slice(),
        [Effect::ConnectConnector {
            provider: Linear,
            ..
        }]
    ));
    driver.send(Input::ActionResult {
        kind: ActionKind::ConnectConnector(ws, Linear),
        result: Ok(ActionOutcome::Connector(Box::new(empty_status(Linear)))),
    });
    assert!(driver.changes.contains(&Change::ConnectorChanged {
        workspace: ws,
        provider: Linear
    }));
    assert!(driver.core.connector(ws, Linear).is_some());
    assert!(driver.core.connector(ws, Jira).is_none());
    driver.send(Input::SignOut);
    assert!(driver.core.connector(ws, Linear).is_none());
}

#[test]
fn a_pasted_secret_is_checked_and_never_shown() {
    let (world, mut driver) = admin();
    let ws = world.workspace.id;
    let kind = ActionKind::SetConnectorSecret(ws, Linear);
    driver.send(Input::SetConnectorSecret {
        workspace: ws,
        provider: Linear,
        secret: SecretUrl::new("has spaces in it".into()),
    });
    assert!(connector_effects(&driver).is_empty());
    assert!(failed(&driver, kind).contains("signing secret"));

    let out = driver.send(Input::SetConnectorSecret {
        workspace: ws,
        provider: Linear,
        secret: SecretUrl::new(format!("  {PASTED}\n")),
    });
    match connector_effects(&driver).as_slice() {
        [Effect::SetConnectorSecret { secret, .. }] => assert_eq!(secret.as_str(), PASTED),
        other => panic!("{other:?}"),
    }
    for text in [format!("{out:?}"), format!("{:?}", driver.effects)] {
        assert!(!text.contains("pasted-9c3e1f"), "{text}");
    }
    // Once set, the status is loaded again; nothing of the secret is kept.
    driver.effects.clear();
    driver.send(Input::ActionResult {
        kind,
        result: Ok(ActionOutcome::Done),
    });
    assert!(matches!(
        connector_effects(&driver).as_slice(),
        [Effect::LoadConnector {
            provider: Linear,
            ..
        }]
    ));
    assert!(driver.core.connector_secret(Linear).is_none());
}

#[test]
fn a_made_secret_is_held_for_its_provider_until_dismissed() {
    let (world, mut driver) = admin();
    let ws = world.workspace.id;
    driver.send(Input::GenerateConnectorSecret {
        workspace: ws,
        provider: Jira,
    });
    assert!(matches!(
        connector_effects(&driver).as_slice(),
        [Effect::GenerateConnectorSecret { provider: Jira, .. }]
    ));
    let out = driver.send(Input::ActionResult {
        kind: ActionKind::SetConnectorSecret(ws, Jira),
        result: Ok(ActionOutcome::ConnectorSecret(SecretUrl::new(MADE.into()))),
    });
    assert_eq!(
        driver.core.connector_secret(Jira).map(SecretUrl::as_str),
        Some(MADE)
    );
    assert!(driver.core.connector_secret(Linear).is_none());
    assert!(driver.changes.contains(&Change::ConnectorSecretChanged));
    for text in [
        format!("{out:?}"),
        format!("{:?}", driver.core.connector_secret(Jira)),
    ] {
        assert!(!text.contains("0f1e2d3c"), "{text}");
    }
    driver.send(Input::DismissConnectorSecret);
    assert!(driver.core.connector_secret(Jira).is_none());
}

#[test]
fn a_subscription_is_checked_before_asking_and_the_status_reloads_after_it() {
    let (world, mut driver) = admin();
    let ws = world.workspace.id;
    let general = world.channel.id;
    let kind = ActionKind::ConnectorSubscribe(general, Linear);
    driver.send(Input::ConnectorSubscribe {
        conversation: general,
        provider: Linear,
        scope: "not a key".into(),
        events: vec!["issue_created".into()],
    });
    assert!(failed(&driver, kind).contains("team key"));
    driver.send(Input::ConnectorSubscribe {
        conversation: general,
        provider: Linear,
        scope: "eng".into(),
        events: vec!["nonsense".into()],
    });
    assert!(failed(&driver, kind).contains("at least one"));
    assert!(connector_effects(&driver).is_empty());

    driver.send(Input::ConnectorSubscribe {
        conversation: general,
        provider: Linear,
        scope: "https://linear.app/acme/team/eng".into(),
        events: vec!["issue_created".into(), "nonsense".into()],
    });
    match connector_effects(&driver).as_slice() {
        [Effect::ConnectorSubscribe { request, .. }] => {
            assert_eq!(request.scope, "ENG");
            assert_eq!(request.events, ["issue_created"]);
        }
        other => panic!("{other:?}"),
    }
    driver.effects.clear();
    driver.send(Input::ActionResult {
        kind,
        result: Ok(ActionOutcome::Done),
    });
    assert!(matches!(
        connector_effects(&driver).as_slice(),
        [Effect::LoadConnector { workspace, provider: Linear }] if *workspace == ws
    ));
}
