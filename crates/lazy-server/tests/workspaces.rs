//! Several workspaces per person (D23): creating a workspace, joining one with an invitation as an
//! existing account, which workspace registration lands in, and — the main risk — that nothing of
//! one workspace ever reaches a person or a subscriber who is not in it.

mod common;

use common::*;
use futures_util::future::join_all;
use lazy_domain::{EventSeq, WorkspaceId, WorkspaceRole};
use lazy_protocol::{ApiError, ErrorCode, Event, JoinedWorkspace, ServerFrame};
use lazy_server::{PasswordCost, RetentionPolicy, ServerOptions};
use sqlx::PgPool;
use uuid::Uuid;

async fn denied(response: reqwest::Response, code: ErrorCode) {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
}

/// The kinds and audiences of a workspace's events after `after`, in order.
async fn log_after(app: &App, workspace: WorkspaceId, after: EventSeq) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT kind, audience FROM events WHERE workspace_id = $1 AND seq > $2 ORDER BY seq",
    )
    .bind(workspace.as_uuid())
    .bind(after.get())
    .fetch_all(&app.pool)
    .await
    .unwrap()
}

async fn count(app: &App, sql: &'static str, bind: Uuid) -> i64 {
    sqlx::query_scalar(sql)
        .bind(bind)
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

// --- Creating ---------------------------------------------------------------------------------

#[sqlx::test]
async fn creating_a_workspace_makes_the_caller_its_owner_with_a_default_channel(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let created = app.create_workspace(&alice, "Team Rocket").await;

    assert_eq!(created.workspace.name, "Team Rocket");
    assert_eq!(created.workspace.settings, Default::default());
    assert_eq!(created.role, WorkspaceRole::Owner);
    assert_eq!(created.head, EventSeq::ZERO);
    assert_eq!(created.conversations.len(), 1);
    let general = &created.conversations[0];
    assert_eq!(general.name.as_ref().unwrap().to_string(), "general");
    assert!(general.is_default);
    assert_eq!(general.member_count, 1);
    assert_eq!(general.workspace_id, created.workspace.id);
    assert_eq!(
        created.members.iter().map(|m| m.id).collect::<Vec<_>>(),
        [alice.user.id]
    );

    // The bootstrap now has both, the seeded one first.
    let boot = app.bootstrap_ok(&alice).await;
    let ids: Vec<_> = boot.workspaces.iter().map(|w| w.workspace.id).collect();
    assert_eq!(ids, [app.workspace_id, created.workspace.id]);
    assert_eq!(boot.workspaces[1], created);

    // Who made it is recorded; the seeded workspace has no creator.
    let made_by: Option<Uuid> =
        sqlx::query_scalar("SELECT created_by FROM workspaces WHERE id = $1")
            .bind(created.workspace.id.as_uuid())
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(made_by, Some(alice.user.id.as_uuid()));
    let seeded: Option<Uuid> =
        sqlx::query_scalar("SELECT created_by FROM workspaces WHERE id = $1")
            .bind(app.workspace_id.as_uuid())
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(seeded, None);

    // The owner can manage it: invite someone.
    let invite = app
        .create_invitation_raw(&alice, created.workspace.id, serde_json::json!({}))
        .await;
    assert!(invite.status().is_success());
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn creating_is_idempotent_by_id_and_anything_else_with_that_id_is_a_conflict(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let id = WorkspaceId::generate();

    let first = app.create_workspace_raw(&alice, id, "Team").await;
    assert!(first.status().is_success());
    let first: JoinedWorkspace = first.json().await.unwrap();
    let again = app.create_workspace_raw(&alice, id, "Team").await;
    assert!(again.status().is_success());
    assert_eq!(again.json::<JoinedWorkspace>().await.unwrap(), first);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM workspaces WHERE id = $1",
            id.as_uuid()
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM workspace_members WHERE workspace_id = $1",
            id.as_uuid()
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM conversations WHERE workspace_id = $1",
            id.as_uuid()
        )
        .await,
        1
    );
    assert_eq!(app.head_of(id).await, EventSeq::ZERO);

    // Another name, another person, and a workspace that already exists are all the same answer.
    let (status, same_person_other_name) =
        api_error(app.create_workspace_raw(&alice, id, "Other name").await).await;
    assert_error(status, &same_person_other_name, ErrorCode::Conflict);
    for response in [
        app.create_workspace_raw(&bob, id, "Team").await,
        app.create_workspace_raw(&bob, id, "Other name").await,
        app.create_workspace_raw(&bob, app.workspace_id, "Lazy")
            .await,
        app.create_workspace_raw(&alice, app.workspace_id, "Lazy")
            .await,
    ] {
        let (status, error) = api_error(response).await;
        assert_error(status, &error, ErrorCode::Conflict);
        assert_eq!(error, same_person_other_name);
    }
    // Nothing was created or changed by the refusals, and bob still has only the seeded one.
    let boot = app.bootstrap_ok(&bob).await;
    assert_eq!(boot.workspaces.len(), 1);

    // A name that is not a workspace name is refused before anything is made.
    let fresh = WorkspaceId::generate();
    denied(
        app.create_workspace_raw(&alice, fresh, "").await,
        ErrorCode::InvalidRequest,
    )
    .await;
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM workspaces WHERE id = $1",
            fresh.as_uuid()
        )
        .await,
        0
    );
    // Without signing in, nothing.
    let anonymous = app
        .http
        .put(app.url(&lazy_protocol::api::paths::workspace(fresh)))
        .json(&serde_json::json!({"name": "x"}))
        .send()
        .await
        .unwrap();
    denied(anonymous, ErrorCode::Unauthenticated).await;
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_creator_who_was_deactivated_is_not_handed_the_workspace_back(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let id = WorkspaceId::generate();
    assert!(
        app.create_workspace_raw(&alice, id, "Team")
            .await
            .status()
            .is_success()
    );
    let invite = app
        .invitation_to(&alice, id, serde_json::json!({"max_uses": 1}))
        .await;
    app.accept(&bob, invite.code.expose()).await;
    assert!(
        app.set_role_raw(&alice, id, bob.user.id, "owner")
            .await
            .status()
            .is_success()
    );
    assert!(
        app.set_deactivated_raw(&bob, id, alice.user.id, true)
            .await
            .status()
            .is_success()
    );
    // She is the creator, but has no membership now: the same answer as for anyone else.
    denied(
        app.create_workspace_raw(&alice, id, "Team").await,
        ErrorCode::Conflict,
    )
    .await;
    // Reactivated, the repeat works again.
    assert!(
        app.set_deactivated_raw(&bob, id, alice.user.id, false)
            .await
            .status()
            .is_success()
    );
    assert!(
        app.create_workspace_raw(&alice, id, "Team")
            .await
            .status()
            .is_success()
    );
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_person_can_create_ten_workspaces_and_not_an_eleventh(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let mut last = WorkspaceId::generate();
    for n in 0..lazy_domain::workspace::MAX_WORKSPACES_CREATED {
        last = WorkspaceId::generate();
        let response = app
            .create_workspace_raw(&alice, last, &format!("W{n}"))
            .await;
        assert!(response.status().is_success(), "workspace {n}");
    }
    let eleventh = WorkspaceId::generate();
    denied(
        app.create_workspace_raw(&alice, eleventh, "One too many")
            .await,
        ErrorCode::LimitReached,
    )
    .await;
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM workspaces WHERE id = $1",
            eleventh.as_uuid()
        )
        .await,
        0
    );
    // Repeating one of the ten is not a new creation.
    assert!(
        app.create_workspace_raw(&alice, last, "W9")
            .await
            .status()
            .is_success()
    );
    // The limit is the person's own: the seeded workspace does not count, and bob has his.
    assert!(
        app.create_workspace_raw(&bob, eleventh, "Bob's")
            .await
            .status()
            .is_success()
    );
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn creating_at_the_same_moment_cannot_exceed_the_limit(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let responses = join_all((0..16).map(|n| {
        let (app, alice) = (&app, &alice);
        async move {
            app.create_workspace_raw(alice, WorkspaceId::generate(), &format!("W{n}"))
                .await
        }
    }))
    .await;
    let mut created = 0;
    for response in responses {
        if response.status().is_success() {
            created += 1;
        } else {
            let (status, error) = api_error(response).await;
            assert_error(status, &error, ErrorCode::LimitReached);
        }
    }
    assert_eq!(created, lazy_domain::workspace::MAX_WORKSPACES_CREATED);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM workspaces WHERE created_by = $1",
            alice.user.id.as_uuid()
        )
        .await,
        lazy_domain::workspace::MAX_WORKSPACES_CREATED as i64
    );
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn two_people_choosing_the_same_id_at_once_leave_one_workspace(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let id = WorkspaceId::generate();
    let (a, b) = tokio::join!(
        app.create_workspace_raw(&alice, id, "Team"),
        app.create_workspace_raw(&bob, id, "Team"),
    );
    let (ok, conflict) = match (a.status().is_success(), b.status().is_success()) {
        (true, false) => (a, b),
        (false, true) => (b, a),
        other => panic!("exactly one must win, got {other:?}"),
    };
    assert!(ok.status().is_success());
    denied(conflict, ErrorCode::Conflict).await;
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM workspace_members WHERE workspace_id = $1",
            id.as_uuid()
        )
        .await,
        1
    );
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn creation_can_be_switched_off_but_joining_still_works(pool: PgPool) {
    let app = App::start_with(
        pool,
        ServerOptions {
            open_registration: true,
            open_workspace_creation: false,
            password_cost: PasswordCost::FAST_FOR_TESTS,
            retention: RetentionPolicy::disabled(),
        },
    )
    .await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let id = WorkspaceId::generate();
    denied(
        app.create_workspace_raw(&alice, id, "Team").await,
        ErrorCode::Forbidden,
    )
    .await;
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM workspaces WHERE id = $1",
            id.as_uuid()
        )
        .await,
        0
    );
    // A workspace that exists (made directly) can still be invited to and joined.
    let elsewhere = app.other_workspace(&[(&alice, "owner")]).await;
    let invite = app
        .invitation_to(&alice, elsewhere, serde_json::json!({}))
        .await;
    let joined = app.accept(&bob, invite.code.expose()).await;
    assert_eq!(joined.workspace.id, elsewhere);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn registration_without_an_invitation_still_lands_in_the_seeded_workspace(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let team = app.create_workspace(&alice, "Team").await;
    let second = app.create_workspace(&alice, "Second").await;

    let carol = app.register("carol").await;
    let boot = app.bootstrap_ok(&carol).await;
    let ids: Vec<_> = boot.workspaces.iter().map(|w| w.workspace.id).collect();
    assert_eq!(ids, [app.workspace_id]);
    // The people made workspaces know nothing of her.
    for workspace in [&team, &second] {
        let members = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
        )
        .bind(workspace.workspace.id.as_uuid())
        .bind(carol.user.id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
        assert_eq!(members, 0);
    }
    // And an invitation to one of them still takes her there, and only there.
    let invite = app
        .invitation_to(
            &alice,
            team.workspace.id,
            serde_json::json!({"max_uses": 1}),
        )
        .await;
    let response = app
        .try_register_with("dave", PASSWORD, Some(invite.code.expose()))
        .await;
    assert!(response.status().is_success());
    let session: lazy_protocol::SessionResponse = response.json().await.unwrap();
    let dave = Account {
        token: session.token.expose().to_owned(),
        user: session.user,
    };
    let ids: Vec<_> = app
        .bootstrap_ok(&dave)
        .await
        .workspaces
        .iter()
        .map(|w| w.workspace.id)
        .collect();
    assert_eq!(ids, [team.workspace.id]);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_workspace_a_person_made_is_never_where_an_open_registration_lands(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let team = app.create_workspace(&alice, "Team").await;
    // The seeded workspace goes away (no server does this; it leaves only a made one).
    sqlx::query("DELETE FROM workspaces WHERE created_by IS NULL")
        .execute(&app.pool)
        .await
        .unwrap();
    // Someone with no invitation is not put into the one alice made.
    denied(
        app.try_register("mallory", PASSWORD).await,
        ErrorCode::Forbidden,
    )
    .await;
    let strangers = count(
        &app,
        "SELECT count(*) FROM workspace_members WHERE workspace_id = $1",
        team.workspace.id.as_uuid(),
    )
    .await;
    assert_eq!(strangers, 1);
    app.server.shutdown().await.unwrap();
}

// --- Accepting an invitation ------------------------------------------------------------------

#[sqlx::test]
async fn accepting_joins_the_workspace_and_announces_it_exactly_as_registration_does(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let team = app.create_workspace(&alice, "Team").await;
    let id = team.workspace.id;
    let general = team.conversations[0].id;

    let mut ws = app.connect_ws(&alice).await;
    ws.subscribe(id, team.head).await;
    ws.replay().await;

    let invite = app
        .invitation_to(&alice, id, serde_json::json!({"max_uses": 2}))
        .await;
    let joined = app.accept(&bob, invite.code.expose()).await;
    assert_eq!(joined.workspace.id, id);
    assert_eq!(joined.role, WorkspaceRole::Member);
    assert!(joined.head > team.head);
    assert_eq!(
        joined.members.iter().map(|m| m.id).collect::<Vec<_>>(),
        [alice.user.id, bob.user.id]
    );
    assert_eq!(joined.conversations.len(), 1);
    assert_eq!(joined.conversations[0].id, general);
    assert_eq!(joined.conversations[0].member_count, 2);
    // The state a client subscribes after: his own bootstrap says the same.
    let boot = app.bootstrap_ok(&bob).await;
    assert_eq!(boot.workspaces.len(), 2);
    assert_eq!(boot.workspaces[1], joined);

    // Alice, in the workspace, is told: someone joined, and is in #general.
    let events = ws.events_through(joined.head).await;
    assert_eq!(events.len(), 2);
    match (&events[0].event, &events[1].event) {
        (
            Event::MemberJoined { user },
            Event::MemberAdded {
                conversation,
                user_id,
            },
        ) => {
            assert_eq!(user.id, bob.user.id);
            assert_eq!(*user_id, bob.user.id);
            assert_eq!(conversation.id, general);
            assert_eq!(conversation.member_count, 2);
        }
        other => panic!("unexpected events {other:?}"),
    }

    // Registration with the same invitation announces the same two, with the same audiences.
    let after_accept = app.head_of(id).await;
    let registered = app
        .try_register_with("carol", PASSWORD, Some(invite.code.expose()))
        .await;
    assert!(registered.status().is_success());
    let accepted_kinds = log_after(&app, id, team.head).await;
    let accepted_kinds = &accepted_kinds[..2];
    let registered_kinds = log_after(&app, id, after_accept).await;
    assert_eq!(accepted_kinds, registered_kinds);
    assert_eq!(
        registered_kinds,
        [
            ("member_joined".to_owned(), "workspace".to_owned()),
            ("member_added".to_owned(), "members".to_owned())
        ]
    );
    // Two uses between them.
    let uses: i32 = sqlx::query_scalar("SELECT uses FROM workspace_invitations WHERE id = $1")
        .bind(invite.invitation.id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(uses, 2);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn the_four_ways_an_invitation_fails_are_one_answer(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let team = app.create_workspace(&alice, "Team").await;
    let id = team.workspace.id;

    let revoked = app.invitation_to(&alice, id, serde_json::json!({})).await;
    let response = app
        .call(
            reqwest::Method::DELETE,
            &lazy_protocol::api::paths::invitation(revoked.invitation.id),
            &alice,
            None,
        )
        .await;
    assert!(response.status().is_success());
    let expired = app.invitation_to(&alice, id, serde_json::json!({})).await;
    sqlx::query(
        "UPDATE workspace_invitations SET expires_at = now() - interval '1 minute' WHERE id = $1",
    )
    .bind(expired.invitation.id.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();
    let used_up = app
        .invitation_to(&alice, id, serde_json::json!({"max_uses": 1}))
        .await;
    let carol = app.register("carol").await;
    app.accept(&carol, used_up.code.expose()).await;
    let unknown = format!("lzy_inv_{}", "A".repeat(43));

    let mut answers: Vec<(u16, ApiError)> = Vec::new();
    for code in [
        revoked.code.expose(),
        expired.code.expose(),
        used_up.code.expose(),
        unknown.as_str(),
        "not a code",
        "",
        "../../etc/passwd",
    ] {
        answers.push(api_error(app.accept_raw(&bob, code).await).await);
    }
    for (status, error) in &answers {
        assert_error(*status, error, ErrorCode::InvitationInvalid);
    }
    assert!(
        answers.windows(2).all(|pair| pair[0] == pair[1]),
        "the answers differ: {answers:?}"
    );
    // None of them joined bob, used anything, or announced anything.
    assert_eq!(app.bootstrap_ok(&bob).await.workspaces.len(), 1);
    let uses: i32 = sqlx::query_scalar("SELECT uses FROM workspace_invitations WHERE id = $1")
        .bind(used_up.invitation.id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(uses, 1);
    // Not signed in: that is a different question.
    let anonymous = app
        .http
        .post(app.url(lazy_protocol::api::paths::ACCEPT_INVITATION_ROUTE))
        .json(&serde_json::json!({"code": used_up.code.expose()}))
        .send()
        .await
        .unwrap();
    denied(anonymous, ErrorCode::Unauthenticated).await;
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn someone_who_already_belongs_is_handed_the_workspace_and_nothing_is_used(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let team = app.create_workspace(&alice, "Team").await;
    let id = team.workspace.id;
    let invite = app
        .invitation_to(&alice, id, serde_json::json!({"max_uses": 1}))
        .await;
    let first = app.accept(&bob, invite.code.expose()).await;
    let head = app.head_of(id).await;
    // Again (the use is spent, but he belongs): the same workspace, no event, no use.
    let again = app.accept(&bob, invite.code.expose()).await;
    assert_eq!(again, first);
    assert_eq!(app.head_of(id).await, head);
    let uses: i32 = sqlx::query_scalar("SELECT uses FROM workspace_invitations WHERE id = $1")
        .bind(invite.invitation.id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(uses, 1);
    // The owner presenting a code of her own workspace gets it too.
    let own = app.accept(&alice, invite.code.expose()).await;
    assert_eq!(own.workspace.id, id);
    assert_eq!(own.role, WorkspaceRole::Owner);
    assert_eq!(app.head_of(id).await, head);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_deactivated_member_is_forbidden_and_uses_nothing(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let team = app.create_workspace(&alice, "Team").await;
    let id = team.workspace.id;
    let invite = app
        .invitation_to(&alice, id, serde_json::json!({"max_uses": 5}))
        .await;
    app.accept(&bob, invite.code.expose()).await;
    assert!(
        app.set_deactivated_raw(&alice, id, bob.user.id, true)
            .await
            .status()
            .is_success()
    );
    let head = app.head_of(id).await;
    denied(
        app.accept_raw(&bob, invite.code.expose()).await,
        ErrorCode::Forbidden,
    )
    .await;
    assert_eq!(app.head_of(id).await, head);
    let uses: i32 = sqlx::query_scalar("SELECT uses FROM workspace_invitations WHERE id = $1")
        .bind(invite.invitation.id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(uses, 1);
    // Still deactivated: the workspace is not his.
    assert_eq!(app.bootstrap_ok(&bob).await.workspaces.len(), 1);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_one_use_invitation_goes_to_exactly_one_of_many_at_once(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let team = app.create_workspace(&alice, "Team").await;
    let id = team.workspace.id;
    let mut people = Vec::new();
    for n in 0..8 {
        people.push(app.register(&format!("person{n}")).await);
    }
    let invite = app
        .invitation_to(&alice, id, serde_json::json!({"max_uses": 1}))
        .await;
    let responses = join_all(people.iter().map(|person| {
        let (app, code) = (&app, invite.code.expose());
        async move { app.accept_raw(person, code).await }
    }))
    .await;
    let mut winners = 0;
    for response in responses {
        if response.status().is_success() {
            winners += 1;
        } else {
            denied(response, ErrorCode::InvitationInvalid).await;
        }
    }
    assert_eq!(winners, 1);
    let uses: i32 = sqlx::query_scalar("SELECT uses FROM workspace_invitations WHERE id = $1")
        .bind(invite.invitation.id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(uses, 1);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM workspace_members WHERE workspace_id = $1",
            id.as_uuid()
        )
        .await,
        2
    );
    let joined = log_after(&app, id, EventSeq::ZERO)
        .await
        .iter()
        .filter(|(kind, _)| kind == "member_joined")
        .count();
    assert_eq!(joined, 1);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn accepting_and_registering_on_one_invitation_at_once_take_turns_without_deadlock(
    pool: PgPool,
) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let team = app.create_workspace(&alice, "Team").await;
    let id = team.workspace.id;
    let mut people = Vec::new();
    for n in 0..6 {
        people.push(app.register(&format!("person{n}")).await);
    }
    for round in 0..3 {
        let invite = app
            .invitation_to(&alice, id, serde_json::json!({"max_uses": 3}))
            .await;
        let code = invite.code.expose();
        let accepting = join_all(
            people
                .iter()
                .filter(|person| {
                    round == 0 || round == 1 || person.user.username.as_str() < "person3"
                })
                .map(|person| {
                    let app = &app;
                    async move { app.accept_raw(person, code).await }
                }),
        );
        let registering = join_all((0..6).map(|n| {
            let app = &app;
            async move {
                app.try_register_with(&format!("fresh{round}x{n}"), PASSWORD, Some(code))
                    .await
            }
        }));
        let (accepted, registered) = tokio::join!(accepting, registering);
        for response in accepted.into_iter().chain(registered) {
            let status = response.status().as_u16();
            assert!(status < 500, "a server error under contention: {status}");
        }
        let uses: i32 = sqlx::query_scalar("SELECT uses FROM workspace_invitations WHERE id = $1")
            .bind(invite.invitation.id.as_uuid())
            .fetch_one(&app.pool)
            .await
            .unwrap();
        assert!(uses <= 3, "{uses} uses of a 3-use invitation");
    }
    app.server.shutdown().await.unwrap();
}

// --- Isolation --------------------------------------------------------------------------------

/// Three workspaces that share people only as listed: A (the seeded one: alice, bob, carol),
/// B (made by alice; erin joined it by registering with its invitation, so is in B only),
/// C (made by carol; frank is in C only).
struct Three {
    app: App,
    alice: Account,
    carol: Account,
    erin: Account,
    frank: Account,
    a_general: lazy_domain::ConversationId,
    b: lazy_protocol::WorkspaceBootstrap,
    c: lazy_protocol::WorkspaceBootstrap,
}

async fn three(pool: PgPool) -> Three {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let _bob = app.register("bob").await;
    let carol = app.register("carol").await;
    let b = app.create_workspace(&alice, "B").await;
    let c = app.create_workspace(&carol, "C").await;
    let mut only = Vec::new();
    for (name, owner, workspace) in [("erin", &alice, &b), ("frank", &carol, &c)] {
        let invite = app
            .invitation_to(owner, workspace.workspace.id, serde_json::json!({}))
            .await;
        let response = app
            .try_register_with(name, PASSWORD, Some(invite.code.expose()))
            .await;
        assert!(response.status().is_success());
        let session: lazy_protocol::SessionResponse = response.json().await.unwrap();
        only.push(Account {
            token: session.token.expose().to_owned(),
            user: session.user,
        });
    }
    let frank = only.pop().unwrap();
    let erin = only.pop().unwrap();
    let a_general = app.general;
    Three {
        app,
        alice,
        carol,
        erin,
        frank,
        a_general,
        b,
        c,
    }
}

#[sqlx::test]
async fn a_person_never_lists_searches_or_fetches_anything_of_a_workspace_they_are_not_in(
    pool: PgPool,
) {
    let t = three(pool).await;
    let app = &t.app;
    let c_id = t.c.workspace.id;
    let c_general = t.c.conversations[0].id;
    let secret = app
        .send(&t.frank, c_general, "needle-from-c in the c workspace")
        .await;
    app.send(&t.alice, t.a_general, "needle-from-a").await;
    let attachment = app
        .upload(
            &t.frank,
            c_general,
            png_bytes(40, 30, 255),
            "image/png",
            "c.png",
        )
        .await;
    let emoji = app
        .upload_emoji(&t.carol, c_id, "cee", png_bytes(32, 32, 255))
        .await;
    let avatar = app.set_avatar(&t.frank, png_bytes(64, 64, 255)).await;
    assert_eq!(avatar.avatar_version, 1);

    // The bootstrap of a person in A and B (alice) is A and B, and nothing of C.
    let boot = app.bootstrap_ok(&t.alice).await;
    let ids: Vec<_> = boot.workspaces.iter().map(|w| w.workspace.id).collect();
    assert_eq!(ids, [app.workspace_id, t.b.workspace.id]);
    let json = serde_json::to_string(&boot).unwrap();
    for needle in [
        c_id.to_string(),
        c_general.to_string(),
        t.frank.user.id.to_string(),
        t.frank.user.username.to_string(),
        secret.id.to_string(),
    ] {
        assert!(
            !json.contains(&needle),
            "alice's bootstrap mentions {needle}"
        );
    }
    // Alice's own searches of A and B do not find C's message; searching C is "not found".
    for workspace in [app.workspace_id, t.b.workspace.id] {
        let response = app
            .search_raw(&t.alice, workspace, &[("q", "needle")])
            .await;
        assert!(response.status().is_success());
        let found: lazy_protocol::SearchResponse = response.json().await.unwrap();
        let text = serde_json::to_string(&found).unwrap();
        assert!(!text.contains("needle-from-c"), "{text}");
    }
    denied(
        app.search_raw(&t.alice, c_id, &[("q", "needle")]).await,
        ErrorCode::NotFound,
    )
    .await;
    // Everything else of C: history, a message, people, a person, the inbox, a file, an emoji.
    denied(
        app.history(&t.alice, c_general, &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.get_message_raw(&t.alice, secret.id).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.people_raw(&t.alice, c_id, &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.person_raw(&t.alice, c_id, t.frank.user.id).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.inbox_raw(&t.alice, c_id, &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.download_raw(&t.alice, attachment.id, false, false, &[])
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.emoji_raw(&t.alice, emoji.id, reqwest::Method::GET, None, &[])
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.create_invitation_raw(&t.alice, c_id, serde_json::json!({}))
            .await,
        ErrorCode::NotFound,
    )
    .await;
    // Nor the directory of A, B or C shows people of another workspace. Alice's A directory has
    // carol (they share A) but not frank; frank's C directory has carol, not alice.
    let people = app.people(&t.alice, &[]).await;
    let names: Vec<String> = people
        .people
        .iter()
        .map(|p| p.user.username.to_string())
        .collect();
    assert_eq!(names, ["alice", "bob", "carol"]);
    // A picture is seen only through a shared workspace.
    denied(
        app.avatar_raw(&t.alice, t.frank.user.id, Some(1), &[])
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.avatar_raw(&t.erin, t.frank.user.id, Some(1), &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    assert!(
        app.avatar_raw(&t.carol, t.frank.user.id, Some(1), &[])
            .await
            .status()
            .is_success()
    );
    // Carol (A and C) sees A and C, not B.
    let boot = app.bootstrap_ok(&t.carol).await;
    let ids: Vec<_> = boot.workspaces.iter().map(|w| w.workspace.id).collect();
    assert_eq!(ids, [app.workspace_id, c_id]);
    assert!(
        !serde_json::to_string(&boot)
            .unwrap()
            .contains(&t.erin.user.id.to_string())
    );
    t.app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn events_of_one_workspace_never_reach_a_subscriber_of_another(pool: PgPool) {
    let t = three(pool).await;
    let app = &t.app;
    let (b_id, c_id) = (t.b.workspace.id, t.c.workspace.id);
    let b_general = t.b.conversations[0].id;
    let c_general = t.c.conversations[0].id;

    // erin is in B only; frank in C only. Each listens to their own workspace from now on.
    let mut erin = app.connect_ws(&t.erin).await;
    erin.subscribe(b_id, app.head_of(b_id).await).await;
    erin.replay().await;
    let mut frank = app.connect_ws(&t.frank).await;
    frank.subscribe(c_id, app.head_of(c_id).await).await;
    frank.replay().await;

    // Activity in A, B and C, and profile changes by alice (A and B) and carol (A and C).
    app.send(&t.alice, t.a_general, "only for A").await;
    app.send(&t.alice, b_general, "only for B").await;
    app.send(&t.carol, c_general, "only for C").await;
    app.rename_me(&t.alice, "Alice Renamed").await;
    app.rename_me(&t.carol, "Carol Renamed").await;

    // The markers: someone joining B, and someone joining C, ends each listener's wait. Everything
    // committed before them has been delivered by then, in order.
    let mut markers = Vec::new();
    for (owner, workspace) in [(&t.alice, b_id), (&t.carol, c_id)] {
        let invite = app
            .invitation_to(owner, workspace, serde_json::json!({}))
            .await;
        let response = app
            .try_register_with(
                &format!("marker{}", markers.len()),
                PASSWORD,
                Some(invite.code.expose()),
            )
            .await;
        let session: lazy_protocol::SessionResponse = response.json().await.unwrap();
        markers.push(session.user.id);
    }
    let for_erin = erin.events_until_marker(markers[0]).await;
    let for_frank = frank.events_until_marker(markers[1]).await;

    // Erin heard B's message and alice's rename, and nothing else; frank, C's and carol's.
    let summary = |events: &[lazy_protocol::EventEnvelope]| -> Vec<String> {
        events
            .iter()
            .map(|e| match &e.event {
                Event::MessageCreated { message, .. } => {
                    format!("message {}", message.document.plain_text())
                }
                Event::UserProfileChanged { display_name, .. } => {
                    format!("profile {display_name}")
                }
                Event::MemberJoined { .. } | Event::MemberAdded { .. } => "membership".to_owned(),
                other => panic!("unexpected event for a one-workspace listener: {other:?}"),
            })
            .collect()
    };
    assert_eq!(
        summary(&for_erin),
        ["message only for B", "profile Alice Renamed"]
    );
    assert_eq!(
        summary(&for_frank),
        ["message only for C", "profile Carol Renamed"]
    );

    // The logs: each rename is in the logs of the workspaces the person belongs to, and in no other.
    let a = app.workspace_id;
    for (workspace, who, expect) in [
        (a, &t.alice, 1usize),
        (b_id, &t.alice, 1),
        (c_id, &t.alice, 0),
        (a, &t.carol, 1),
        (c_id, &t.carol, 1),
        (b_id, &t.carol, 0),
    ] {
        let events = app.profile_events(workspace, EventSeq::ZERO).await;
        let mine = events.iter().filter(|e| e.1 == who.user.id).count();
        assert_eq!(mine, expect, "{} in {workspace}", who.user.username);
    }

    // A listener cannot subscribe to a workspace they are not in, whatever the cursor.
    let mut erin_other = app.connect_ws(&t.erin).await;
    for workspace in [app.workspace_id, c_id] {
        erin_other.subscribe(workspace, EventSeq::ZERO).await;
        match erin_other.next().await {
            ServerFrame::Error { code, .. } => assert_eq!(code, ErrorCode::Forbidden),
            other => panic!("unexpected frame {other:?}"),
        }
    }
    t.app.server.shutdown().await.unwrap();
}
