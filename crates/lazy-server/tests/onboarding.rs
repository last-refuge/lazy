//! What a new client asks before anyone has signed in (D29): `GET /v1/server`, and
//! `POST /v1/invitations/preview`, which says what a code is for without using it. Also the new
//! invitation code format: readable, forgiving to type, and the older codes still working.

mod common;

use common::*;
use lazy_domain::workspace::{
    RegistrationPolicy, WorkspaceSettings, canonical_invite_code, format_invite_code,
};
use lazy_protocol::api::paths;
use lazy_protocol::{
    ErrorCode, InvitationKind, InvitationPreview, ServerInfoResponse, ServerRegistration,
    SessionResponse,
};
use lazy_server::db::{self, SeededWorkspace};
use lazy_server::testing;
use lazy_server::{IntegrationOptions, PasswordCost, RetentionPolicy, ServerOptions, SetupCode};
use sqlx::PgPool;

const SETUP_CODE: &str = "setup-code-for-tests-0123456789";
const INVALID_MESSAGE: &str = "that invitation is not valid";

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

async fn registered(response: reqwest::Response) -> Account {
    assert!(response.status().is_success(), "{}", response.status());
    let session: SessionResponse = response.json().await.unwrap();
    Account {
        token: session.token.expose().to_owned(),
        user: session.user,
    }
}

async fn info(app: &App) -> ServerInfoResponse {
    let response = app
        .http
        .get(app.url(paths::SERVER_INFO))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    response.json().await.unwrap()
}

async fn preview_raw(app: &App, code: &str) -> reqwest::Response {
    app.http
        .post(app.url(paths::PREVIEW_INVITATION))
        .json(&serde_json::json!({ "code": code }))
        .send()
        .await
        .unwrap()
}

async fn preview(app: &App, code: &str) -> InvitationPreview {
    let response = preview_raw(app, code).await;
    assert!(response.status().is_success(), "{}", response.status());
    response.json().await.unwrap()
}

/// The answer to a code that cannot be used: always this one.
async fn assert_invalid(response: reqwest::Response) {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, ErrorCode::InvitationInvalid);
    assert_eq!(error.message, INVALID_MESSAGE);
}

#[sqlx::test]
async fn the_server_says_how_it_takes_accounts_and_nothing_else(pool: PgPool) {
    let app = start(pool, RegistrationPolicy::Open, None, true).await;
    let response = app
        .http
        .get(app.url(paths::SERVER_INFO))
        .send()
        .await
        .unwrap();
    let raw: serde_json::Value = response.json().await.unwrap();
    let mut keys: Vec<&str> = raw
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["protocol_version", "registration"],
        "no name, no count, no hint that the server is unclaimed"
    );
    let open = info(&app).await;
    assert_eq!(open.protocol_version, lazy_protocol::PROTOCOL_VERSION);
    assert_eq!(open.registration, ServerRegistration::Open);
    assert!(!open.setup_code_required);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn an_invitation_only_server_says_so_once_it_has_a_member(pool: PgPool) {
    let app = start(pool, RegistrationPolicy::InviteOnly, None, true).await;
    // Empty, it lets the first person in as its owner (nobody could have invited them), so for
    // that person it is open.
    assert_eq!(info(&app).await.registration, ServerRegistration::Open);
    app.register("owner").await;
    let after = info(&app).await;
    assert_eq!(after.registration, ServerRegistration::InviteOnly);
    assert!(!after.setup_code_required);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_server_waiting_for_its_setup_code_says_so_until_the_first_account_exists(pool: PgPool) {
    let app = start(pool, RegistrationPolicy::Open, Some(SETUP_CODE), true).await;
    let waiting = info(&app).await;
    assert!(waiting.setup_code_required);
    assert_eq!(
        waiting.registration,
        ServerRegistration::InviteOnly,
        "a code is needed, and the setup code goes where an invitation code does"
    );
    registered(
        app.try_register_with("owner", PASSWORD, Some(SETUP_CODE))
            .await,
    )
    .await;
    let claimed = info(&app).await;
    assert!(!claimed.setup_code_required);
    assert_eq!(claimed.registration, ServerRegistration::Open);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_server_that_takes_no_accounts_says_so_and_previews_nothing(pool: PgPool) {
    let app = start(pool, RegistrationPolicy::Open, Some(SETUP_CODE), false).await;
    let closed = info(&app).await;
    assert_eq!(closed.registration, ServerRegistration::Closed);
    // Not even the setup code is previewed: registration would refuse it.
    let (status, error) = api_error(preview_raw(&app, SETUP_CODE).await).await;
    assert_error(status, &error, ErrorCode::Forbidden);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_valid_code_names_the_workspace_and_who_invited_and_uses_nothing(pool: PgPool) {
    let app = App::start(pool).await;
    let owner = app.register("owner").await;
    let created = app.create_invitation(&owner, serde_json::json!({})).await;
    let code = created.code.expose().to_owned();

    let seen = preview(&app, &code).await;
    assert_eq!(seen.kind, InvitationKind::Invitation);
    assert_eq!(seen.workspace_name, "Lazy");
    assert_eq!(
        seen.inviter.as_ref().map(|name| name.as_str()),
        Some("User owner")
    );
    assert_eq!(seen.member_count, 1);

    // Asking any number of times uses nothing.
    for _ in 0..3 {
        preview(&app, &code).await;
    }
    let listed = app.list_invitations(&owner).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].uses, 0);

    // Registering with it does use it, and the next look counts the new person.
    registered(
        app.try_register_with("newcomer", PASSWORD, Some(&code))
            .await,
    )
    .await;
    assert_eq!(app.list_invitations(&owner).await[0].uses, 1);
    assert_eq!(preview(&app, &code).await.member_count, 2);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_code_may_be_written_any_way_a_person_copies_it(pool: PgPool) {
    let app = App::start(pool).await;
    let owner = app.register("owner").await;
    let code = app
        .create_invitation(&owner, serde_json::json!({}))
        .await
        .code
        .expose()
        .to_owned();
    assert_eq!(code.len(), 20, "{code}");
    let grouped = format_invite_code(&code);
    assert_eq!(grouped.len(), 24);
    assert_eq!(canonical_invite_code(&grouped), code);

    for written in [
        grouped.clone(),
        grouped.to_lowercase(),
        grouped.replace('-', " "),
        format!("  {grouped}\n"),
        grouped.replace('-', "\u{2013}"),
    ] {
        let seen = preview(&app, &written).await;
        assert_eq!(seen.workspace_name, "Lazy", "{written:?}");
    }

    // Registering and accepting read it the same way.
    let newcomer = registered(
        app.try_register_with("newcomer", PASSWORD, Some(&grouped.to_lowercase()))
            .await,
    )
    .await;
    assert_eq!(app.bootstrap_ok(&newcomer).await.workspaces.len(), 1);
    let other = app.register("other").await;
    // `other` joined the default workspace already (registration is open), so accepting returns it
    // without using the invitation; the point here is that the grouped code is recognized.
    app.accept(&other, &grouped).await;
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_code_that_cannot_be_used_is_one_answer_whatever_the_reason(pool: PgPool) {
    let app = App::start(pool).await;
    let owner = app.register("owner").await;

    let revoked = app.create_invitation(&owner, serde_json::json!({})).await;
    let response = app
        .revoke_invitation_raw(&owner, revoked.invitation.id)
        .await;
    assert!(response.status().is_success(), "{}", response.status());

    let expired = app.create_invitation(&owner, serde_json::json!({})).await;
    sqlx::query(
        "UPDATE workspace_invitations SET expires_at = now() - interval '1 minute' WHERE id = $1",
    )
    .bind(expired.invitation.id.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();

    let once = app
        .create_invitation(&owner, serde_json::json!({ "max_uses": 1 }))
        .await;
    registered(
        app.try_register_with("only", PASSWORD, Some(once.code.expose()))
            .await,
    )
    .await;

    let unknown = lazy_domain::workspace::invite_code_from_entropy(&[0x5a; 13]);
    let too_long = "A".repeat(65);
    let codes = [
        revoked.code.expose().to_owned(),
        expired.code.expose().to_owned(),
        once.code.expose().to_owned(),
        unknown,
        String::new(),
        "short".to_owned(),
        too_long,
        "not a code, with spaces and ünïcode ☃".to_owned(),
        SETUP_CODE.to_owned(),
    ];
    for code in &codes {
        assert_invalid(preview_raw(&app, code).await).await;
    }
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn the_setup_code_previews_as_setup_only_while_the_server_waits_for_it(pool: PgPool) {
    let app = start(pool, RegistrationPolicy::Open, Some(SETUP_CODE), true).await;
    let seen = preview(&app, SETUP_CODE).await;
    assert_eq!(seen.kind, InvitationKind::Setup);
    assert_eq!(seen.workspace_name, "Lazy");
    assert_eq!(seen.inviter, None);
    assert_eq!(seen.member_count, 0);
    // Close, but not it: the same answer as any code that is nothing.
    for wrong in [
        format!("{SETUP_CODE}x"),
        SETUP_CODE.to_uppercase(),
        format!(" {SETUP_CODE}"),
        SETUP_CODE[..SETUP_CODE.len() - 1].to_owned(),
    ] {
        assert_invalid(preview_raw(&app, &wrong).await).await;
    }
    registered(
        app.try_register_with("owner", PASSWORD, Some(SETUP_CODE))
            .await,
    )
    .await;
    // Once the first account exists, the setup code is nothing.
    assert_invalid(preview_raw(&app, SETUP_CODE).await).await;
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn codes_issued_before_the_new_format_still_work_and_are_never_rewritten(pool: PgPool) {
    let app = App::start(pool).await;
    let owner = app.register("owner").await;
    let legacy = "AbC-def_GHI0123456789jklMNOPqrst";
    assert_eq!(legacy.len(), 32);
    let hash = lazy_server::crypto::invite_code_hash(legacy);
    sqlx::query(
        "INSERT INTO workspace_invitations (id, workspace_id, code_hash, created_by, expires_at) \
         VALUES ($1, $2, $3, $4, now() + interval '1 day')",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(app.workspace_id.as_uuid())
    .bind(&hash[..])
    .bind(owner.user.id.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();

    let seen = preview(&app, legacy).await;
    assert_eq!(seen.kind, InvitationKind::Invitation);
    assert_eq!(seen.workspace_name, "Lazy");
    // Surrounding space is forgiven; nothing else about it is.
    preview(&app, &format!("  {legacy}\n")).await;
    assert_invalid(preview_raw(&app, &legacy.to_lowercase()).await).await;
    registered(
        app.try_register_with("newcomer", PASSWORD, Some(legacy))
            .await,
    )
    .await;
    app.server.shutdown().await.unwrap();
}
