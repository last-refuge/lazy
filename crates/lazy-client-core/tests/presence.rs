//! Presence (D45): who is online, as the live connection says, and unknown while it is down.

mod common;

use common::{Driver, World};
use lazy_client_core::{Change, Input, SocketEvent};
use lazy_protocol::ServerFrame;

#[test]
fn presence_follows_the_connection_and_is_unknown_without_it() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let workspace = world.workspace.id;
    let (me, zoe) = (world.me.id, world.other.id);
    assert_eq!(driver.core.is_online(zoe), None, "unknown before any word");

    driver.send(Input::Socket(SocketEvent::Opened { workspace }));
    driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: workspace,
        seq: world.head,
    }));
    let out = driver.send(Input::Frame(ServerFrame::Presence {
        workspace_id: workspace,
        online: vec![me],
    }));
    assert!(out.changes.contains(&Change::PresenceChanged));
    assert_eq!(driver.core.is_online(me), Some(true));
    assert_eq!(driver.core.is_online(zoe), Some(false));

    // Zoe comes online, then goes.
    let out = driver.send(Input::Frame(ServerFrame::PresenceChanged {
        workspace_id: workspace,
        user_id: zoe,
        online: true,
    }));
    assert!(out.changes.contains(&Change::PresenceChanged));
    assert_eq!(driver.core.is_online(zoe), Some(true));
    driver.send(Input::Frame(ServerFrame::PresenceChanged {
        workspace_id: workspace,
        user_id: zoe,
        online: false,
    }));
    assert_eq!(driver.core.is_online(zoe), Some(false));

    // The connection drops: nobody's presence is known until it says again.
    let out = driver.send(Input::Socket(SocketEvent::Closed {
        workspace,
        attempt: 1,
    }));
    assert!(out.changes.contains(&Change::PresenceChanged));
    assert_eq!(driver.core.is_online(zoe), None);
    // A change for a list it does not hold is ignored.
    driver.send(Input::Frame(ServerFrame::PresenceChanged {
        workspace_id: workspace,
        user_id: zoe,
        online: true,
    }));
    assert_eq!(driver.core.is_online(zoe), None);
}
