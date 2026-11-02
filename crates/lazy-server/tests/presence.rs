//! Presence (D45): who is online, told on the realtime connection, never stored.

mod common;

use std::time::Duration;

use common::*;
use lazy_domain::UserId;
use lazy_protocol::ServerFrame;
use lazy_server::IntegrationOptions;
use sqlx::PgPool;

async fn start(pool: PgPool, grace: Duration) -> App {
    App::start_with_integrations(
        pool,
        IntegrationOptions {
            presence_grace: Some(grace),
            ..IntegrationOptions::default()
        },
    )
    .await
}

/// The next presence frame, skipping events and `caught_up`.
async fn presence(ws: &mut Ws) -> ServerFrame {
    loop {
        let frame = ws.next_frame_or_closed().await.expect("an open socket");
        if let ServerFrame::Presence { .. } | ServerFrame::PresenceChanged { .. } = frame {
            return frame;
        }
    }
}

async fn subscribed(app: &App, who: &Account) -> Ws {
    let mut ws = app.connect_ws(who).await;
    ws.subscribe(app.workspace_id, app.head().await).await;
    ws
}

#[sqlx::test]
async fn people_are_online_while_connected_and_offline_after_a_grace_period(pool: PgPool) {
    let app = start(pool, Duration::from_millis(300)).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;

    // Alice connects: after catching up she is told who is online, herself included.
    let mut alice_ws = subscribed(&app, &alice).await;
    match presence(&mut alice_ws).await {
        ServerFrame::Presence {
            workspace_id,
            online,
        } => {
            assert_eq!(workspace_id, app.workspace_id);
            assert_eq!(online, vec![alice.user.id]);
        }
        other => panic!("expected the list first, got {other:?}"),
    }

    // Bob connects: Alice hears he is online; Bob's own list has them both.
    let mut bob_ws = subscribed(&app, &bob).await;
    assert_eq!(
        presence(&mut alice_ws).await,
        ServerFrame::PresenceChanged {
            workspace_id: app.workspace_id,
            user_id: bob.user.id,
            online: true,
        }
    );
    match presence(&mut bob_ws).await {
        ServerFrame::Presence { online, .. } => {
            let mut expected: Vec<UserId> = vec![alice.user.id, bob.user.id];
            expected.sort();
            assert_eq!(online, expected);
        }
        other => panic!("expected the list, got {other:?}"),
    }

    // Bob goes: after the grace period Alice hears he is offline.
    drop(bob_ws);
    assert_eq!(
        presence(&mut alice_ws).await,
        ServerFrame::PresenceChanged {
            workspace_id: app.workspace_id,
            user_id: bob.user.id,
            online: false,
        }
    );
}

#[sqlx::test]
async fn reconnecting_within_the_grace_period_does_not_flicker(pool: PgPool) {
    let app = start(pool, Duration::from_secs(30)).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let mut alice_ws = subscribed(&app, &alice).await;
    presence(&mut alice_ws).await;
    let bob_ws = subscribed(&app, &bob).await;
    assert!(matches!(
        presence(&mut alice_ws).await,
        ServerFrame::PresenceChanged { online: true, .. }
    ));

    // Bob's connection drops and comes back: Alice hears nothing about it.
    drop(bob_ws);
    let _bob_again = subscribed(&app, &bob).await;
    let marker = app.register("marker").await;
    let mut marker_ws = subscribed(&app, &marker).await;
    // The marker's arrival proves earlier changes would have been delivered by now, and nothing
    // about Bob came before it.
    match presence(&mut alice_ws).await {
        ServerFrame::PresenceChanged {
            user_id, online, ..
        } if user_id == marker.user.id => assert!(online),
        other => panic!("nothing about Bob was expected, got {other:?}"),
    }
    match presence(&mut marker_ws).await {
        ServerFrame::Presence { online, .. } => assert!(online.contains(&bob.user.id)),
        other => panic!("expected the list, got {other:?}"),
    }
}
