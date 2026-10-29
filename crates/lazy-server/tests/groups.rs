//! User groups: the lifecycle, who may manage them, idempotent creation, the one namespace for
//! handles (and its races with registration), limits, events, the members endpoint, and bootstrap.

mod common;

use std::time::Duration;

use common::*;
use lazy_domain::{
    GroupId, MAX_GROUP_MEMBERS, MAX_GROUPS_PER_WORKSPACE, OperationId, UserId, WorkspaceId,
};
use lazy_protocol::{ErrorCode, Event, EventEnvelope, OperationStatus, RegisterRequest, UserGroup};
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Barrier;

async fn denied(response: reqwest::Response, code: ErrorCode) {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
}

async fn subscribed(app: &App, who: &Account) -> Ws {
    let mut ws = app.connect_ws(who).await;
    ws.subscribe(app.workspace_id, app.head().await).await;
    ws.replay().await;
    ws
}

fn sorted(mut users: Vec<UserId>) -> Vec<UserId> {
    users.sort();
    users
}

/// An owner, an admin, and three ordinary members.
struct Chat {
    app: App,
    owner: Account,
    boss: Account,
    alice: Account,
    bob: Account,
    carol: Account,
}

async fn chat(pool: PgPool) -> Chat {
    let app = App::start(pool).await;
    let owner = app.register("owner").await;
    let boss = app.register("boss").await;
    app.set_role(&boss, "admin").await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let carol = app.register("carol").await;
    Chat {
        app,
        owner,
        boss,
        alice,
        bob,
        carol,
    }
}

async fn finish(chat: Chat) {
    chat.app.server.shutdown().await.unwrap();
}

fn group_changes(events: &[EventEnvelope]) -> Vec<&UserGroup> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::GroupChanged { group } => Some(group),
            _ => None,
        })
        .collect()
}

#[sqlx::test]
async fn admins_create_list_change_and_delete_groups(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let mut member_ws = subscribed(app, &c.alice).await;
    let mut admin_ws = subscribed(app, &c.boss).await;

    // Create: the response is the group, members in a fixed order, duplicates gone.
    let response = app
        .create_group_raw(
            &c.boss,
            "design-team",
            "Design Team",
            &[&c.bob, &c.alice, &c.bob],
            OperationId::generate(),
        )
        .await;
    assert!(response.status().is_success());
    let created = response
        .json::<lazy_protocol::GroupResponse>()
        .await
        .unwrap()
        .group;
    assert_eq!(created.handle.as_str(), "design-team");
    assert_eq!(created.name.as_str(), "Design Team");
    assert_eq!(created.workspace_id, app.workspace_id);
    assert_eq!(
        created.members,
        sorted(vec![c.alice.user.id, c.bob.user.id])
    );

    // Anyone in the workspace can list it, and bootstrap carries it.
    assert_eq!(app.list_groups(&c.carol).await, vec![created.clone()]);
    assert_eq!(
        app.bootstrap_ok(&c.carol).await.workspaces[0].groups,
        vec![created.clone()]
    );

    // Change the handle, the name, and the whole membership.
    let changed = app
        .update_group_raw(
            &c.owner,
            created.id,
            json!({"handle": "ux", "name": "UX", "members": [c.carol.user.id]}),
        )
        .await;
    assert!(changed.status().is_success());
    let changed = changed
        .json::<lazy_protocol::GroupResponse>()
        .await
        .unwrap()
        .group;
    assert_eq!(
        (changed.handle.as_str(), changed.name.as_str()),
        ("ux", "UX")
    );
    assert_eq!(changed.members, vec![c.carol.user.id]);
    assert_eq!(app.list_groups(&c.bob).await, vec![changed.clone()]);

    // Absent fields are unchanged; asking for what already holds changes and announces nothing.
    let head = app.head().await;
    let renamed = app
        .update_group_raw(&c.boss, created.id, json!({"name": "UX Team"}))
        .await;
    let renamed = renamed
        .json::<lazy_protocol::GroupResponse>()
        .await
        .unwrap()
        .group;
    assert_eq!(
        (renamed.handle.as_str(), renamed.members.clone()),
        ("ux", changed.members.clone())
    );
    let after_rename = app.head().await;
    assert!(after_rename > head);
    for same in [
        json!({}),
        json!({"handle": "ux"}),
        json!({"members": [c.carol.user.id]}),
        json!({"name": "UX Team"}),
    ] {
        assert!(
            app.update_group_raw(&c.boss, created.id, same)
                .await
                .status()
                .is_success()
        );
    }
    assert_eq!(app.head().await, after_rename);

    // Delete, twice: the second finds nothing to do and says so quietly.
    assert_eq!(
        app.delete_group_raw(&c.boss, created.id)
            .await
            .status()
            .as_u16(),
        204
    );
    assert_eq!(
        app.delete_group_raw(&c.boss, created.id)
            .await
            .status()
            .as_u16(),
        204
    );
    assert!(app.list_groups(&c.alice).await.is_empty());

    // The whole workspace heard each change, with the complete group, and the deletion.
    let marker = app.register("marker").await;
    for ws in [&mut member_ws, &mut admin_ws] {
        let events = ws.events_until_marker(marker.user.id).await;
        let groups = group_changes(&events);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0], &created);
        assert_eq!(groups[1], &changed);
        assert_eq!(groups[2], &renamed);
        let deleted: Vec<GroupId> = events
            .iter()
            .filter_map(|e| match &e.event {
                Event::GroupDeleted { group_id } => Some(*group_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            deleted,
            vec![created.id],
            "the second delete announced nothing"
        );
    }
    finish(c).await;
}

#[sqlx::test]
async fn only_admins_and_owners_manage_groups_and_other_workspaces_are_invisible(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let group = app.create_group(&c.boss, "design", &[&c.alice]).await;

    // An ordinary member can read but not write.
    denied(
        app.create_group_raw(&c.bob, "mine", "Mine", &[], OperationId::generate())
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.update_group_raw(&c.bob, group.id, json!({"name": "Mine"}))
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.delete_group_raw(&c.bob, group.id).await,
        ErrorCode::Forbidden,
    )
    .await;
    assert_eq!(app.list_groups(&c.bob).await.len(), 1);

    // Someone outside the workspace sees nothing of it.
    let outsider = app.register("outsider").await;
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
        .bind(outsider.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    let listing = app
        .call(
            reqwest::Method::GET,
            &lazy_protocol::api::paths::groups(app.workspace_id),
            &outsider,
            None,
        )
        .await;
    denied(listing, ErrorCode::NotFound).await;
    denied(
        app.create_group_raw(&outsider, "mine", "Mine", &[], OperationId::generate())
            .await,
        ErrorCode::NotFound,
    )
    .await;

    // A group in another workspace is not found, even for an admin here.
    let elsewhere: WorkspaceId = app.other_workspace(&[(&c.alice, "owner")]).await;
    let foreign = GroupId::generate();
    sqlx::query(
        "INSERT INTO user_groups (id, workspace_id, handle, name) VALUES ($1, $2, 'away', 'Away')",
    )
    .bind(foreign.as_uuid())
    .bind(elsewhere.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();
    denied(
        app.update_group_raw(&c.boss, foreign, json!({"name": "Mine"}))
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.update_group_raw(&c.bob, foreign, json!({"name": "Mine"}))
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.delete_group_raw(&c.boss, foreign).await,
        ErrorCode::NotFound,
    )
    .await;
    // One that never existed behaves like the update; deleting it is the quiet no-op.
    denied(
        app.update_group_raw(&c.boss, GroupId::generate(), json!({"name": "x"}))
            .await,
        ErrorCode::NotFound,
    )
    .await;
    assert_eq!(
        app.delete_group_raw(&c.boss, GroupId::generate())
            .await
            .status()
            .as_u16(),
        204
    );
    let still_there: i64 = sqlx::query_scalar("SELECT count(*) FROM user_groups WHERE id = $1")
        .bind(foreign.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(still_there, 1);
    finish(c).await;
}

#[sqlx::test]
async fn creating_a_group_is_idempotent_and_its_operation_id_cannot_be_reused(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let mut ws = subscribed(app, &c.alice).await;
    let operation = OperationId::generate();

    let first = app
        .create_group_raw(&c.boss, "design", "Design", &[&c.alice, &c.bob], operation)
        .await;
    let first = first
        .json::<lazy_protocol::GroupResponse>()
        .await
        .unwrap()
        .group;
    let head = app.head().await;
    let again = app
        .create_group_raw(&c.boss, "design", "Design", &[&c.bob, &c.alice], operation)
        .await;
    assert_eq!(
        again
            .json::<lazy_protocol::GroupResponse>()
            .await
            .unwrap()
            .group,
        first,
        "member order does not matter"
    );
    assert_eq!(app.head().await, head, "no second group, no second event");

    // A lost response is reconciled through the status; operation ids are per person.
    match app.operation(&c.boss, operation).await {
        OperationStatus::GroupCommitted { group } => assert_eq!(group, first),
        other => panic!("unexpected status {other:?}"),
    }
    assert_eq!(
        app.operation(&c.owner, operation).await,
        OperationStatus::NotFound
    );

    // The same id for anything else is a conflict.
    denied(
        app.create_group_raw(
            &c.boss,
            "design",
            "Other name",
            &[&c.alice, &c.bob],
            operation,
        )
        .await,
        ErrorCode::Conflict,
    )
    .await;
    denied(
        app.create_group_raw(&c.boss, "design", "Design", &[&c.alice], operation)
            .await,
        ErrorCode::Conflict,
    )
    .await;
    denied(
        app.create_group_raw(&c.boss, "other", "Design", &[&c.alice, &c.bob], operation)
            .await,
        ErrorCode::Conflict,
    )
    .await;
    denied(
        app.create_channel_raw(&c.boss, "channel-x", "public", operation)
            .await,
        ErrorCode::Conflict,
    )
    .await;

    // Once the group is deleted the receipt remains: a repeat must not make a new group.
    assert_eq!(
        app.delete_group_raw(&c.boss, first.id)
            .await
            .status()
            .as_u16(),
        204
    );
    denied(
        app.create_group_raw(&c.boss, "design", "Design", &[&c.alice, &c.bob], operation)
            .await,
        ErrorCode::Conflict,
    )
    .await;
    assert_eq!(
        app.operation(&c.boss, operation).await,
        OperationStatus::NotFound
    );
    assert!(app.list_groups(&c.boss).await.is_empty());

    let marker = app.register("marker").await;
    let events = ws.events_until_marker(marker.user.id).await;
    assert_eq!(
        group_changes(&events).len(),
        1,
        "one group was ever created"
    );
    finish(c).await;
}

#[sqlx::test]
async fn handles_are_one_namespace_for_people_groups_and_reserved_words(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let design = app.create_group(&c.boss, "design", &[]).await;

    // A group cannot take a username, a reserved word, or another group's handle.
    denied(
        app.create_group_raw(&c.boss, "alice", "Alice", &[], OperationId::generate())
            .await,
        ErrorCode::NameTaken,
    )
    .await;
    denied(
        app.create_group_raw(&c.boss, "design", "Again", &[], OperationId::generate())
            .await,
        ErrorCode::NameTaken,
    )
    .await;
    for reserved in ["channel", "here", "everyone", "Channel"] {
        denied(
            app.create_group_raw(&c.boss, reserved, "Nope", &[], OperationId::generate())
                .await,
            ErrorCode::InvalidRequest,
        )
        .await;
    }
    for malformed in ["a", "-leading", "trailing-", "has space", "bad!", ""] {
        denied(
            app.create_group_raw(&c.boss, malformed, "Nope", &[], OperationId::generate())
                .await,
            ErrorCode::InvalidRequest,
        )
        .await;
    }
    // Handles are lowercase whatever was typed, so a different case is the same handle.
    denied(
        app.create_group_raw(&c.boss, "DESIGN", "Shouting", &[], OperationId::generate())
            .await,
        ErrorCode::NameTaken,
    )
    .await;
    denied(
        app.create_group_raw(&c.boss, "Alice", "Alice", &[], OperationId::generate())
            .await,
        ErrorCode::NameTaken,
    )
    .await;
    // Renaming is held to the same rules, and renaming to itself is not a clash.
    let ux = app.create_group(&c.boss, "ux", &[]).await;
    denied(
        app.update_group_raw(&c.boss, ux.id, json!({"handle": "bob"}))
            .await,
        ErrorCode::NameTaken,
    )
    .await;
    denied(
        app.update_group_raw(&c.boss, ux.id, json!({"handle": "design"}))
            .await,
        ErrorCode::NameTaken,
    )
    .await;
    denied(
        app.update_group_raw(&c.boss, ux.id, json!({"handle": "channel"}))
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    assert!(
        app.update_group_raw(&c.boss, ux.id, json!({"handle": "ux"}))
            .await
            .status()
            .is_success()
    );
    assert!(
        app.update_group_raw(&c.boss, ux.id, json!({"handle": "ux-team"}))
            .await
            .status()
            .is_success()
    );
    // A freed handle can be used again.
    assert!(
        app.create_group_raw(&c.boss, "ux", "UX again", &[], OperationId::generate())
            .await
            .status()
            .is_success()
    );

    // A new account cannot take a group's handle (as the username is taken) or a reserved word.
    let taken = app.try_register("design", PASSWORD).await;
    denied(taken, ErrorCode::UsernameTaken).await;
    for reserved in ["channel", "here", "everyone"] {
        denied(
            app.try_register(reserved, PASSWORD).await,
            ErrorCode::InvalidRequest,
        )
        .await;
    }
    // Once the group is deleted, the name is free for a person.
    assert_eq!(
        app.delete_group_raw(&c.boss, design.id)
            .await
            .status()
            .as_u16(),
        204
    );
    assert!(
        app.try_register("design", PASSWORD)
            .await
            .status()
            .is_success()
    );

    // The same handle in another workspace is a different group.
    let elsewhere = app.other_workspace(&[(&c.boss, "admin")]).await;
    let created = app
        .call(
            reqwest::Method::POST,
            &lazy_protocol::api::paths::groups(elsewhere),
            &c.boss,
            Some(json!({"operation_id": OperationId::generate(), "handle": "ux-team", "name": "Elsewhere UX", "members": []})),
        )
        .await;
    assert!(created.status().is_success(), "{}", created.status());
    finish(c).await;
}

/// A registration or group creation that arrives while another transaction holds a handle's lock
/// must wait for it, and then see what that transaction did. The test holds the lock itself, so the
/// outcome does not depend on timing.
#[sqlx::test]
async fn registration_waits_for_a_group_that_is_claiming_the_same_handle(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let mut claim = app.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('handle:rival', 0))")
        .execute(&mut *claim)
        .await
        .unwrap();

    let body = RegisterRequest {
        username: lazy_domain::Username::parse("rival").unwrap(),
        display_name: lazy_domain::DisplayName::parse("Rival").unwrap(),
        password: lazy_protocol::Password::new(PASSWORD),
        invite_code: None,
    };
    let (http, url) = (
        app.http.clone(),
        app.url(lazy_protocol::api::paths::REGISTER),
    );
    let mut registration =
        tokio::spawn(async move { http.post(url).json(&body).send().await.unwrap() });
    // While the handle is claimed, registration does not get through.
    assert!(
        tokio::time::timeout(Duration::from_millis(600), &mut registration)
            .await
            .is_err(),
        "registration should wait for the handle lock"
    );

    // The claimant takes the handle for a group and commits; the registration then sees it.
    sqlx::query("INSERT INTO user_groups (id, workspace_id, handle, name) VALUES ($1, $2, 'rival', 'Rival')")
        .bind(GroupId::generate().as_uuid())
        .bind(app.workspace_id.as_uuid())
        .execute(&mut *claim)
        .await
        .unwrap();
    claim.commit().await.unwrap();
    let response = tokio::time::timeout(DEADLINE, registration)
        .await
        .expect("it finishes")
        .unwrap();
    denied(response, ErrorCode::UsernameTaken).await;
    let people: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE username = 'rival'")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(people, 0);
    finish(c).await;
}

#[sqlx::test]
async fn creating_a_group_waits_for_a_registration_that_is_claiming_the_same_handle(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let mut claim = app.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('handle:rival', 0))")
        .execute(&mut *claim)
        .await
        .unwrap();

    let (http, url, token) = (
        app.http.clone(),
        app.url(&lazy_protocol::api::paths::groups(app.workspace_id)),
        c.boss.token.clone(),
    );
    let body = json!({"operation_id": OperationId::generate(), "handle": "rival", "name": "Rival", "members": []});
    let mut creation = tokio::spawn(async move {
        http.post(url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap()
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(600), &mut creation)
            .await
            .is_err(),
        "creating a group should wait for the handle lock"
    );

    // The claimant takes the handle for a person and commits; the creation then sees them.
    sqlx::query("INSERT INTO users (id, username, display_name, password_hash) VALUES ($1, 'rival', 'Rival', 'x')")
        .bind(UserId::generate().as_uuid())
        .execute(&mut *claim)
        .await
        .unwrap();
    claim.commit().await.unwrap();
    let response = tokio::time::timeout(DEADLINE, creation)
        .await
        .expect("it finishes")
        .unwrap();
    denied(response, ErrorCode::NameTaken).await;
    assert!(app.list_groups(&c.boss).await.is_empty());
    finish(c).await;
}

#[sqlx::test]
async fn renaming_a_group_waits_for_a_registration_that_is_claiming_the_same_handle(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let group = app.create_group(&c.boss, "before", &[]).await;
    let mut claim = app.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('handle:rival', 0))")
        .execute(&mut *claim)
        .await
        .unwrap();

    let (http, url, token) = (
        app.http.clone(),
        app.url(&lazy_protocol::api::paths::group(group.id)),
        c.boss.token.clone(),
    );
    let mut rename = tokio::spawn(async move {
        http.patch(url)
            .bearer_auth(token)
            .json(&json!({"handle": "rival"}))
            .send()
            .await
            .unwrap()
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(600), &mut rename)
            .await
            .is_err(),
        "renaming a group should wait for the handle lock"
    );

    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash) VALUES ($1, 'rival', 'Rival', 'x')",
    )
    .bind(UserId::generate().as_uuid())
    .execute(&mut *claim)
    .await
    .unwrap();
    claim.commit().await.unwrap();
    let response = tokio::time::timeout(DEADLINE, rename)
        .await
        .expect("it finishes")
        .unwrap();
    denied(response, ErrorCode::NameTaken).await;
    assert_eq!(app.list_groups(&c.boss).await[0].handle.as_str(), "before");
    finish(c).await;
}

#[sqlx::test]
async fn a_registration_and_a_rename_racing_for_a_handle_have_one_winner(pool: PgPool) {
    const ROUNDS: usize = 15;
    let c = chat(pool).await;
    let app = &c.app;
    for round in 0..ROUNDS {
        let handle = format!("swap-{round}");
        let group = app
            .create_group(&c.boss, &format!("old-{round}"), &[])
            .await;
        let start = std::sync::Arc::new(Barrier::new(2));

        let registration = {
            let (http, url, start) = (
                app.http.clone(),
                app.url(lazy_protocol::api::paths::REGISTER),
                start.clone(),
            );
            let body = json!({"username": handle, "display_name": "Racer", "password": PASSWORD});
            tokio::spawn(async move {
                start.wait().await;
                http.post(url)
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            })
        };
        let rename = {
            let (http, url, token, start) = (
                app.http.clone(),
                app.url(&lazy_protocol::api::paths::group(group.id)),
                c.boss.token.clone(),
                start.clone(),
            );
            let body = json!({"handle": handle});
            tokio::spawn(async move {
                start.wait().await;
                http.patch(url)
                    .bearer_auth(token)
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            })
        };
        let (registered, renamed) = tokio::time::timeout(DEADLINE, async {
            (registration.await.unwrap(), rename.await.unwrap())
        })
        .await
        .expect("the round finishes");
        assert!(
            (registered == 200) != (renamed == 200),
            "round {round}: exactly one may win, got registration {registered}, rename {renamed}"
        );
        let people: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE username = $1")
            .bind(&handle)
            .fetch_one(&app.pool)
            .await
            .unwrap();
        let groups: i64 = sqlx::query_scalar("SELECT count(*) FROM user_groups WHERE handle = $1")
            .bind(&handle)
            .fetch_one(&app.pool)
            .await
            .unwrap();
        assert_eq!(
            people + groups,
            1,
            "round {round}: the handle names one thing"
        );
    }
    finish(c).await;
}

/// The same race without help from the test: both sides go at once, over and over, and exactly one
/// can ever win.
#[sqlx::test]
async fn a_registration_and_a_group_racing_for_a_handle_have_one_winner(pool: PgPool) {
    const ROUNDS: usize = 15;
    let c = chat(pool).await;
    let app = &c.app;
    for round in 0..ROUNDS {
        let handle = format!("race-{round}");
        let start = std::sync::Arc::new(Barrier::new(2));

        let registration = {
            let (http, url, start) = (
                app.http.clone(),
                app.url(lazy_protocol::api::paths::REGISTER),
                start.clone(),
            );
            let body = json!({"username": handle, "display_name": "Racer", "password": PASSWORD});
            tokio::spawn(async move {
                start.wait().await;
                http.post(url)
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            })
        };
        let creation = {
            let (http, url, token, start) = (
                app.http.clone(),
                app.url(&lazy_protocol::api::paths::groups(app.workspace_id)),
                c.boss.token.clone(),
                start.clone(),
            );
            let body = json!({"operation_id": OperationId::generate(), "handle": handle, "name": "Racing", "members": []});
            tokio::spawn(async move {
                start.wait().await;
                http.post(url)
                    .bearer_auth(token)
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            })
        };
        let (registered, created) = tokio::time::timeout(DEADLINE, async {
            (registration.await.unwrap(), creation.await.unwrap())
        })
        .await
        .expect("the round finishes");
        assert!(
            (registered == 200) != (created == 200),
            "round {round}: exactly one may win, got registration {registered}, group {created}"
        );
        assert!([200, 409].contains(&registered) && [200, 409].contains(&created));

        let people: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE username = $1")
            .bind(&handle)
            .fetch_one(&app.pool)
            .await
            .unwrap();
        let groups: i64 = sqlx::query_scalar("SELECT count(*) FROM user_groups WHERE handle = $1")
            .bind(&handle)
            .fetch_one(&app.pool)
            .await
            .unwrap();
        assert_eq!(
            people + groups,
            1,
            "round {round}: the handle names one thing"
        );
    }
    finish(c).await;
}

#[sqlx::test]
async fn creating_and_renaming_to_one_handle_at_once_has_one_winner(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    for round in 0..8 {
        let target = format!("target-{round}");
        let existing = app
            .create_group(&c.boss, &format!("old-{round}"), &[])
            .await;
        let start = std::sync::Arc::new(Barrier::new(2));
        let create = {
            let (http, url, token, start, handle) = (
                app.http.clone(),
                app.url(&lazy_protocol::api::paths::groups(app.workspace_id)),
                c.boss.token.clone(),
                start.clone(),
                target.clone(),
            );
            tokio::spawn(async move {
                start.wait().await;
                let body = json!({"operation_id": OperationId::generate(), "handle": handle, "name": "New", "members": []});
                http.post(url)
                    .bearer_auth(token)
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            })
        };
        let rename = {
            let (http, url, token, start, handle) = (
                app.http.clone(),
                app.url(&lazy_protocol::api::paths::group(existing.id)),
                c.owner.token.clone(),
                start.clone(),
                target.clone(),
            );
            tokio::spawn(async move {
                start.wait().await;
                http.patch(url)
                    .bearer_auth(token)
                    .json(&json!({"handle": handle}))
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            })
        };
        let (created, renamed) = tokio::time::timeout(DEADLINE, async {
            (create.await.unwrap(), rename.await.unwrap())
        })
        .await
        .expect("the round finishes");
        assert!(
            (created == 200) != (renamed == 200),
            "round {round}: {created} / {renamed}"
        );
        let holders: i64 = sqlx::query_scalar("SELECT count(*) FROM user_groups WHERE handle = $1")
            .bind(&target)
            .fetch_one(&app.pool)
            .await
            .unwrap();
        assert_eq!(holders, 1);
    }
    finish(c).await;
}

#[sqlx::test]
async fn groups_and_their_membership_are_limited(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;

    // A group holds at most 200 people; build a crowd of workspace members directly.
    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash) \
         SELECT ('00000000-0000-0000-0000-' || lpad(to_hex(g), 12, '0'))::uuid, 'crowd' || g, 'Crowd ' || g, 'x' \
         FROM generate_series(1, $1) g",
    )
    .bind(MAX_GROUP_MEMBERS as i32 + 1)
    .execute(&app.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, user_id, role) \
         SELECT $1, ('00000000-0000-0000-0000-' || lpad(to_hex(g), 12, '0'))::uuid, 'member' \
         FROM generate_series(1, $2) g",
    )
    .bind(app.workspace_id.as_uuid())
    .bind(MAX_GROUP_MEMBERS as i32 + 1)
    .execute(&app.pool)
    .await
    .unwrap();
    let crowd: Vec<UserId> = (1..=MAX_GROUP_MEMBERS as u128 + 1)
        .map(|n| UserId::from_uuid(uuid::Uuid::from_u128(n)))
        .collect();

    let too_many = json!({"operation_id": OperationId::generate(), "handle": "crowd", "name": "Crowd", "members": crowd});
    let response = app
        .call(
            reqwest::Method::POST,
            &lazy_protocol::api::paths::groups(app.workspace_id),
            &c.boss,
            Some(too_many),
        )
        .await;
    assert_eq!(response.status().as_u16(), 422);
    denied(response, ErrorCode::LimitReached).await;
    // Exactly 200 is fine, and so is repeating people (they collapse).
    let mut exactly = crowd[..MAX_GROUP_MEMBERS].to_vec();
    exactly.push(crowd[0]);
    let ok = json!({"operation_id": OperationId::generate(), "handle": "crowd", "name": "Crowd", "members": exactly});
    let ok = app
        .call(
            reqwest::Method::POST,
            &lazy_protocol::api::paths::groups(app.workspace_id),
            &c.boss,
            Some(ok),
        )
        .await;
    assert!(ok.status().is_success());
    let made = ok
        .json::<lazy_protocol::GroupResponse>()
        .await
        .unwrap()
        .group;
    assert_eq!(made.members.len(), MAX_GROUP_MEMBERS);
    // Growing it past the limit through an update is refused too.
    let grown = app
        .update_group_raw(&c.boss, made.id, json!({"members": crowd}))
        .await;
    denied(grown, ErrorCode::LimitReached).await;

    // Everyone must be in the workspace.
    let stranger = UserId::generate();
    let outsider = json!({"operation_id": OperationId::generate(), "handle": "strangers", "name": "S", "members": [stranger]});
    let outsider = app
        .call(
            reqwest::Method::POST,
            &lazy_protocol::api::paths::groups(app.workspace_id),
            &c.boss,
            Some(outsider),
        )
        .await;
    denied(outsider, ErrorCode::InvalidRequest).await;
    denied(
        app.update_group_raw(&c.boss, made.id, json!({"members": [stranger]}))
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;

    // A workspace holds at most 100 groups: this one has one already.
    for n in 1..MAX_GROUPS_PER_WORKSPACE {
        app.create_group(&c.boss, &format!("group-{n}"), &[]).await;
    }
    assert_eq!(
        app.list_groups(&c.alice).await.len(),
        MAX_GROUPS_PER_WORKSPACE
    );
    let response = app
        .create_group_raw(
            &c.boss,
            "one-too-many",
            "Too many",
            &[],
            OperationId::generate(),
        )
        .await;
    assert_eq!(response.status().as_u16(), 422);
    denied(response, ErrorCode::LimitReached).await;
    // Deleting one makes room.
    assert_eq!(
        app.delete_group_raw(&c.boss, made.id)
            .await
            .status()
            .as_u16(),
        204
    );
    assert!(
        app.create_group_raw(
            &c.boss,
            "one-too-many",
            "Fits now",
            &[],
            OperationId::generate()
        )
        .await
        .status()
        .is_success()
    );
    finish(c).await;
}

#[sqlx::test]
async fn a_returning_subscriber_catches_up_on_group_changes_in_order(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let cursor = app.head().await;

    let group = app.create_group(&c.boss, "design", &[&c.alice]).await;
    app.update_group_raw(
        &c.boss,
        group.id,
        json!({"members": [c.alice.user.id, c.bob.user.id]}),
    )
    .await;
    let second = app.create_group(&c.boss, "ux", &[]).await;
    app.delete_group_raw(&c.boss, group.id).await;

    // Everyone in the workspace replays the same four group events, whoever they are.
    for who in [&c.alice, &c.carol, &c.boss] {
        let mut ws = app.connect_ws(who).await;
        ws.subscribe(app.workspace_id, cursor).await;
        let (events, _) = ws.replay().await;
        let shapes: Vec<(&str, GroupId)> = events
            .iter()
            .filter_map(|e| match &e.event {
                Event::GroupChanged { group } => Some(("changed", group.id)),
                Event::GroupDeleted { group_id } => Some(("deleted", *group_id)),
                _ => None,
            })
            .collect();
        assert_eq!(
            shapes,
            vec![
                ("changed", group.id),
                ("changed", group.id),
                ("changed", second.id),
                ("deleted", group.id)
            ]
        );
        assert!(events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
    }
    // The stored audience is the whole workspace.
    let audiences: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT audience FROM events WHERE kind IN ('group_changed', 'group_deleted')",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(audiences, vec!["workspace".to_owned()]);
    finish(c).await;
}

#[sqlx::test]
async fn the_members_endpoint_lists_who_is_in_a_conversation_for_anyone_who_can_read_it(
    pool: PgPool,
) {
    let c = chat(pool).await;
    let app = &c.app;
    let members = |conversation: lazy_domain::ConversationId, who: &Account| {
        let (path, who) = (
            lazy_protocol::api::paths::conversation_action(conversation, "members"),
            who.clone(),
        );
        async move { app.call(reqwest::Method::GET, &path, &who, None).await }
    };
    let list = |response: reqwest::Response| async move {
        assert!(response.status().is_success(), "{}", response.status());
        response
            .json::<lazy_protocol::ConversationMembersResponse>()
            .await
            .unwrap()
    };

    // A private channel: its members only, in the order they joined.
    let hush = app.create_channel(&c.alice, "hush", "private").await;
    assert!(
        app.add_members(&c.alice, hush.id, &[&c.bob])
            .await
            .status()
            .is_success()
    );
    let seen = list(members(hush.id, &c.bob).await).await;
    assert_eq!(seen.conversation_id, hush.id);
    assert_eq!(seen.user_ids, vec![c.alice.user.id, c.bob.user.id]);
    for outsider in [&c.carol, &c.boss, &c.owner] {
        denied(members(hush.id, outsider).await, ErrorCode::NotFound).await;
    }

    // A public channel: any workspace member may ask, and learns who is in it.
    let team = app.create_channel(&c.alice, "team", "public").await;
    assert!(app.act(&c.bob, team.id, "join").await.status().is_success());
    let seen = list(members(team.id, &c.carol).await).await;
    assert_eq!(seen.user_ids, vec![c.alice.user.id, c.bob.user.id]);

    // A direct conversation: its two people.
    let direct = app.open(&c.alice, &[&c.bob]).await;
    assert_eq!(
        list(members(direct.id, &c.alice).await)
            .await
            .user_ids
            .len(),
        2
    );
    denied(members(direct.id, &c.carol).await, ErrorCode::NotFound).await;

    // It follows membership changes, and people outside the workspace are shut out.
    assert!(
        app.remove_member(&c.alice, hush.id, &c.bob)
            .await
            .status()
            .is_success()
    );
    assert_eq!(
        list(members(hush.id, &c.alice).await).await.user_ids,
        vec![c.alice.user.id]
    );
    let gone = app.register("gone").await;
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
        .bind(gone.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    denied(members(team.id, &gone).await, ErrorCode::NotFound).await;
    denied(
        members(lazy_domain::ConversationId::generate(), &c.alice).await,
        ErrorCode::NotFound,
    )
    .await;
    finish(c).await;
}

#[sqlx::test]
async fn bootstrap_lists_groups_by_handle_with_their_members(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let zebra = app.create_group(&c.boss, "zebra", &[&c.carol]).await;
    let alpha = app
        .create_group(&c.boss, "alpha", &[&c.bob, &c.alice])
        .await;
    let groups = app.bootstrap_ok(&c.bob).await.workspaces[0].groups.clone();
    assert_eq!(groups, vec![alpha, zebra]);
    finish(c).await;
}
