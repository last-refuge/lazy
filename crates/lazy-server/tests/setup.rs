//! The setup code and the seeded registration policy (a server on a public address must not let a
//! stranger become the owner by registering first).

mod common;

use std::time::Duration;

use common::*;
use futures_util::future::join_all;
use lazy_domain::workspace::{RegistrationPolicy, WorkspaceSettings};
use lazy_domain::{EventSeq, WorkspaceRole};
use lazy_protocol::{ApiError, ErrorCode, SessionResponse};
use lazy_server::db::{self, SeededWorkspace};
use lazy_server::testing;
use lazy_server::{IntegrationOptions, PasswordCost, RetentionPolicy, ServerOptions, SetupCode};
use sqlx::PgPool;

const SETUP_CODE: &str = "setup-code-for-tests-0123456789";
const SETUP_MESSAGE: &str = "this server needs its setup code to create the first account";

fn options(open_registration: bool) -> ServerOptions {
    ServerOptions {
        open_registration,
        open_workspace_creation: true,
        password_cost: PasswordCost::FAST_FOR_TESTS,
        retention: RetentionPolicy::disabled(),
    }
}

/// A server on a database seeded with `policy` for its workspace, with `code` as the setup code.
async fn start(
    pool: PgPool,
    policy: RegistrationPolicy,
    code: Option<&str>,
    open_registration: bool,
) -> App {
    let SeededWorkspace {
        workspace_id,
        general_id,
    } = db::seed_default_workspace_with(
        &pool,
        WorkspaceSettings {
            registration: policy,
            ..WorkspaceSettings::default()
        },
    )
    .await
    .unwrap()
    .expect("a fresh database");
    let integrations = IntegrationOptions {
        setup_code: code.map(|code| SetupCode::new(code).unwrap()),
        ..IntegrationOptions::default()
    };
    let server = testing::spawn_test_server_configured(
        pool.clone(),
        options(open_registration),
        integrations,
    )
    .await;
    App {
        server,
        pool,
        http: reqwest::Client::new(),
        workspace_id,
        general: general_id,
    }
}

async fn account_of(response: reqwest::Response) -> Account {
    assert!(response.status().is_success(), "{}", response.status());
    let session: SessionResponse = response.json().await.unwrap();
    Account {
        token: session.token.expose().to_owned(),
        user: session.user,
    }
}

async fn refused(response: reqwest::Response) -> (u16, ApiError) {
    api_error(response).await
}

async fn members(app: &App) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM workspace_members WHERE workspace_id = $1")
        .bind(app.workspace_id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

async fn role_of(app: &App, who: &Account) -> WorkspaceRole {
    app.bootstrap_ok(who).await.workspaces[0].role
}

#[sqlx::test]
async fn without_a_setup_code_the_first_to_register_owns_the_workspace_as_before(pool: PgPool) {
    let app = start(pool, RegistrationPolicy::Open, None, true).await;
    let first = account_of(app.try_register("first", PASSWORD).await).await;
    assert_eq!(role_of(&app, &first).await, WorkspaceRole::Owner);
    // A code that is nothing is just a wrong invitation.
    let (status, error) = refused(
        app.try_register_with("second", PASSWORD, Some(SETUP_CODE))
            .await,
    )
    .await;
    assert_error(status, &error, ErrorCode::InvitationInvalid);
    let second = account_of(app.try_register("second", PASSWORD).await).await;
    assert_eq!(role_of(&app, &second).await, WorkspaceRole::Member);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn with_a_setup_code_the_first_account_needs_it_and_every_failure_is_one_answer(
    pool: PgPool,
) {
    let app = start(pool, RegistrationPolicy::Open, Some(SETUP_CODE), true).await;
    // Missing, wrong, empty, one character off, the right one with something added, and shapes
    // that no invitation code has: all the same answer, and nothing is created.
    let wrong_ones: Vec<Option<String>> = vec![
        None,
        Some(String::new()),
        Some("wrong-setup-code-0123456789".to_owned()),
        Some(SETUP_CODE[..SETUP_CODE.len() - 1].to_owned()),
        Some(format!("{SETUP_CODE}x")),
        Some(SETUP_CODE.to_uppercase()),
        Some(format!(" {SETUP_CODE}")),
        Some("not a code at all, with spaces and ünïcode ☃".to_owned()),
        Some("lzy_inv_".to_owned() + &"A".repeat(40)),
    ];
    let mut answers = Vec::new();
    for (n, code) in wrong_ones.iter().enumerate() {
        let response = app
            .try_register_with(&format!("stranger{n}"), PASSWORD, code.as_deref())
            .await;
        answers.push(refused(response).await);
    }
    for (status, error) in &answers {
        assert_error(*status, error, ErrorCode::Forbidden);
        assert_eq!(error.message, SETUP_MESSAGE);
    }
    assert!(
        answers.windows(2).all(|pair| pair[0] == pair[1]),
        "{answers:?}"
    );
    assert_eq!(members(&app).await, 0);
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(users, 0);

    // The right code makes the first account, who owns the workspace and is in #general.
    let owner = account_of(
        app.try_register_with("owner", PASSWORD, Some(SETUP_CODE))
            .await,
    )
    .await;
    assert_eq!(role_of(&app, &owner).await, WorkspaceRole::Owner);
    assert_eq!(members(&app).await, 1);
    let boot = app.bootstrap_ok(&owner).await;
    assert_eq!(boot.workspaces[0].conversations.len(), 1);
    assert!(app.try_login("owner", PASSWORD).await.status().is_success());
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_setup_code_of_twenty_hex_digits_is_matched_as_the_app_writes_it(pool: PgPool) {
    // 20 hexadecimal digits are also 20 characters of the invitation alphabet, which the app puts
    // in capitals before it sends them: the server must not hold out for the lower case it was
    // configured in.
    let configured = "0123456789abcdef0123";
    let app = start(pool, RegistrationPolicy::Open, Some(configured), true).await;
    // Padding is not forgiven (the app trims), and a code that is one digit off is nothing.
    for wrong in [" 0123456789ABCDEF0123", "0123456789ABCDEF0124"] {
        let (status, error) =
            refused(app.try_register_with("stranger", PASSWORD, Some(wrong)).await).await;
        assert_error(status, &error, ErrorCode::Forbidden);
        assert_eq!(error.message, SETUP_MESSAGE);
    }
    assert_eq!(members(&app).await, 0);
    let owner = account_of(
        app.try_register_with("owner", PASSWORD, Some("0123456789ABCDEF0123"))
            .await,
    )
    .await;
    assert_eq!(role_of(&app, &owner).await, WorkspaceRole::Owner);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn once_anyone_is_in_the_setup_code_has_no_effect_at_all(pool: PgPool) {
    let app = start(pool, RegistrationPolicy::Open, Some(SETUP_CODE), true).await;
    let owner = account_of(
        app.try_register_with("owner", PASSWORD, Some(SETUP_CODE))
            .await,
    )
    .await;
    // It is not an invitation: a second registration that presents it is as wrong as any other
    // code, and so is an existing account trying to join something with it.
    for code in [SETUP_CODE, "wrong-setup-code-0123456789", "odd shape ☃"] {
        let (status, error) =
            refused(app.try_register_with("late", PASSWORD, Some(code)).await).await;
        assert_error(status, &error, ErrorCode::InvitationInvalid);
    }
    let elsewhere = app.other_workspace(&[(&owner, "owner")]).await;
    let outsider = {
        let invite = app
            .invitation_to(&owner, elsewhere, serde_json::json!({}))
            .await;
        account_of(
            app.try_register_with("outsider", PASSWORD, Some(invite.code.expose()))
                .await,
        )
        .await
    };
    let (status, error) = refused(app.accept_raw(&outsider, SETUP_CODE).await).await;
    assert_error(status, &error, ErrorCode::InvitationInvalid);
    // Not even when the one account that made it a workspace with members is deactivated: the
    // workspace has members still, and the setup is over for good.
    sqlx::query("UPDATE workspace_members SET deactivated_at = now() WHERE user_id = $1")
        .bind(owner.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    let (status, error) = refused(
        app.try_register_with("again", PASSWORD, Some(SETUP_CODE))
            .await,
    )
    .await;
    assert_error(status, &error, ErrorCode::InvitationInvalid);
    sqlx::query("UPDATE workspace_members SET deactivated_at = NULL WHERE user_id = $1")
        .bind(owner.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    // Registration is then as it always was: without a code, an ordinary member.
    let member = account_of(app.try_register("member", PASSWORD).await).await;
    assert_eq!(role_of(&app, &member).await, WorkspaceRole::Member);
    assert_eq!(members(&app).await, 2);
    // The setup message is never given again.
    let (_, error) = refused(app.try_register_with("xylo", PASSWORD, Some("nope")).await).await;
    assert_ne!(error.message, SETUP_MESSAGE);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn while_waiting_for_the_first_account_an_invitation_to_another_workspace_is_not_enough(
    pool: PgPool,
) {
    let app = start(pool, RegistrationPolicy::Open, Some(SETUP_CODE), true).await;
    // Someone with the power to make invitations elsewhere does not exist yet, so the invitation
    // is made by hand: the point is that it is not honoured while the setup code is wanted.
    let creator = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO users (id, username, display_name, password_hash) VALUES ($1, 'maker', 'Maker', 'x')")
        .bind(creator)
        .execute(&app.pool)
        .await
        .unwrap();
    let elsewhere = lazy_domain::WorkspaceId::generate();
    sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, 'Elsewhere')")
        .bind(elsewhere.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    let code = "an-invitation-code-to-elsewhere-123";
    sqlx::query(
        "INSERT INTO workspace_invitations (id, workspace_id, code_hash, created_by, expires_at) \
         VALUES ($1, $2, $3, $4, now() + interval '1 day')",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(elsewhere.as_uuid())
    .bind(&lazy_server::crypto::invite_code_hash(code)[..])
    .bind(creator)
    .execute(&app.pool)
    .await
    .unwrap();
    // The default workspace is empty, so that is a wrong setup code.
    let (status, error) = refused(app.try_register_with("guest", PASSWORD, Some(code)).await).await;
    assert_error(status, &error, ErrorCode::Forbidden);
    assert_eq!(error.message, SETUP_MESSAGE);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn closed_registration_stays_closed_whatever_the_setup_code(pool: PgPool) {
    let app = start(pool, RegistrationPolicy::Open, Some(SETUP_CODE), false).await;
    let (status, error) = refused(
        app.try_register_with("owner", PASSWORD, Some(SETUP_CODE))
            .await,
    )
    .await;
    assert_error(status, &error, ErrorCode::Forbidden);
    assert_eq!(error.message, "registration is closed");
    assert_eq!(members(&app).await, 0);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn of_many_first_registrations_with_the_right_code_exactly_one_owns_the_workspace(
    pool: PgPool,
) {
    let app = start(pool, RegistrationPolicy::Open, Some(SETUP_CODE), true).await;
    let responses = join_all((0..10).map(|n| {
        let app = &app;
        async move {
            app.try_register_with(&format!("racer{n}"), PASSWORD, Some(SETUP_CODE))
                .await
        }
    }))
    .await;
    let mut winners = Vec::new();
    for response in responses {
        if response.status().is_success() {
            winners.push(account_of(response).await);
        } else {
            // The others went through the ordinary path, where the code is a wrong invitation.
            let (status, error) = refused(response).await;
            assert_error(status, &error, ErrorCode::InvitationInvalid);
        }
    }
    assert_eq!(winners.len(), 1);
    assert_eq!(role_of(&app, &winners[0]).await, WorkspaceRole::Owner);
    assert_eq!(members(&app).await, 1);
    let owners: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workspace_members WHERE workspace_id = $1 AND role = 'owner'",
    )
    .bind(app.workspace_id.as_uuid())
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(owners, 1);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn the_first_registration_waits_for_the_workspaces_registration_lock(pool: PgPool) {
    let app = start(pool, RegistrationPolicy::Open, Some(SETUP_CODE), true).await;
    let key = app.workspace_id.to_string();
    let mut held = app.pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
        .bind(&key)
        .execute(&mut *held)
        .await
        .unwrap();
    let response = {
        let request = app.try_register_with("owner", PASSWORD, Some(SETUP_CODE));
        tokio::pin!(request);
        assert!(
            tokio::time::timeout(Duration::from_millis(500), &mut request)
                .await
                .is_err(),
            "the first registration did not wait for the registration lock"
        );
        sqlx::query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
            .bind(&key)
            .execute(&mut *held)
            .await
            .unwrap();
        tokio::time::timeout(DEADLINE, request).await.unwrap()
    };
    drop(held);
    let owner = account_of(response).await;
    assert_eq!(role_of(&app, &owner).await, WorkspaceRole::Owner);
    app.server.shutdown().await.unwrap();
}

// --- The seeded policy ----------------------------------------------------------------------------

#[sqlx::test]
async fn an_invite_only_seed_lets_the_first_account_in_and_then_only_invitations(pool: PgPool) {
    let app = start(pool, RegistrationPolicy::InviteOnly, None, true).await;
    let owner = account_of(app.try_register("owner", PASSWORD).await).await;
    assert_eq!(role_of(&app, &owner).await, WorkspaceRole::Owner);
    let (status, error) = refused(app.try_register("second", PASSWORD).await).await;
    assert_error(status, &error, ErrorCode::Forbidden);
    assert_eq!(error.message, "registration is by invitation");
    let invite = app.create_invitation(&owner, serde_json::json!({})).await;
    let guest = account_of(
        app.try_register_with("guest", PASSWORD, Some(invite.code.expose()))
            .await,
    )
    .await;
    assert_eq!(role_of(&app, &guest).await, WorkspaceRole::Member);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn an_invite_only_seed_with_a_setup_code_needs_the_code_first_and_then_invitations(
    pool: PgPool,
) {
    let app = start(pool, RegistrationPolicy::InviteOnly, Some(SETUP_CODE), true).await;
    let (status, error) = refused(app.try_register("early", PASSWORD).await).await;
    assert_error(status, &error, ErrorCode::Forbidden);
    assert_eq!(error.message, SETUP_MESSAGE);
    let owner = account_of(
        app.try_register_with("owner", PASSWORD, Some(SETUP_CODE))
            .await,
    )
    .await;
    assert_eq!(role_of(&app, &owner).await, WorkspaceRole::Owner);
    let (status, error) = refused(app.try_register("second", PASSWORD).await).await;
    assert_error(status, &error, ErrorCode::Forbidden);
    assert_eq!(error.message, "registration is by invitation");
    // The workspace's settings say what they were seeded as.
    let settings = &app.bootstrap_ok(&owner).await.workspaces[0]
        .workspace
        .settings;
    assert_eq!(settings.registration, RegistrationPolicy::InviteOnly);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn the_seed_policy_applies_only_when_the_workspace_is_created(pool: PgPool) {
    let first = db::seed_default_workspace_with(
        &pool,
        WorkspaceSettings {
            registration: RegistrationPolicy::InviteOnly,
            ..WorkspaceSettings::default()
        },
    )
    .await
    .unwrap();
    assert!(first.is_some());
    // A later start, with another policy: nothing is created and nothing changes.
    let again = db::seed_default_workspace_with(&pool, WorkspaceSettings::default())
        .await
        .unwrap();
    assert!(again.is_none());
    let plain = db::seed_default_workspace(&pool).await.unwrap();
    assert!(plain.is_none());
    let settings: serde_json::Value = sqlx::query_scalar("SELECT settings FROM workspaces")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(settings["registration"], "invite_only");
    let _ = EventSeq::ZERO;
}
