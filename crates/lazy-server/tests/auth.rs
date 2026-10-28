mod common;

use common::*;
use lazy_domain::{OperationId, WorkspaceRole};
use lazy_protocol::{ApiError, ErrorCode, PROTOCOL_VERSION};
use lazy_server::{PasswordCost, RetentionPolicy, ServerOptions};
use sqlx::PgPool;

#[sqlx::test]
async fn register_signs_in_and_joins_the_default_workspace(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    assert!(ada.token.starts_with("lzy_"));

    let snapshot = app.bootstrap_ok(&ada).await;
    assert_eq!(snapshot.protocol_version, PROTOCOL_VERSION);
    assert_eq!(snapshot.user, ada.user);
    assert_eq!(snapshot.workspaces.len(), 1);
    let workspace = &snapshot.workspaces[0];
    assert_eq!(workspace.workspace.id, app.workspace_id);
    assert_eq!(workspace.workspace.name, "Lazy");
    // The first person to register owns the workspace; the next is a member.
    assert_eq!(workspace.role, WorkspaceRole::Owner);
    assert_eq!(workspace.conversations.len(), 1);
    assert_eq!(workspace.conversations[0].id, app.general);
    assert_eq!(
        workspace.conversations[0].name.as_ref().unwrap().as_str(),
        "general"
    );
    assert_eq!(workspace.members, vec![ada.user.clone()]);
    // Registering is two events: joining the workspace, and joining #general.
    assert_eq!(workspace.head.get(), 2);

    let grace = app.register("grace").await;
    let snapshot = app.bootstrap_ok(&grace).await;
    assert_eq!(snapshot.workspaces[0].role, WorkspaceRole::Member);
    assert_eq!(snapshot.workspaces[0].members.len(), 2);
    assert_eq!(snapshot.workspaces[0].head.get(), 4);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn usernames_are_unique_ignoring_case(pool: PgPool) {
    let app = App::start(pool).await;
    app.register("ada").await;
    let (status, error) = api_error(app.try_register("ADA", PASSWORD).await).await;
    assert_error(status, &error, ErrorCode::UsernameTaken);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn weak_and_malformed_registrations_are_rejected(pool: PgPool) {
    let app = App::start(pool).await;
    let (status, error) = api_error(app.try_register("ada", "short").await).await;
    assert_error(status, &error, ErrorCode::InvalidRequest);
    let (status, error) = api_error(app.try_register("ada", &"x".repeat(257)).await).await;
    assert_error(status, &error, ErrorCode::InvalidRequest);

    // An invalid username fails validation while the body is parsed.
    let response = app
        .http
        .post(app.url("/v1/auth/register"))
        .body(r#"{"username":"x","display_name":"X","password":"long enough pw"}"#)
        .header("content-type", "application/json")
        .send()
        .await
        .unwrap();
    let (status, error) = api_error(response).await;
    assert_error(status, &error, ErrorCode::InvalidRequest);

    // Nothing was created by the failed attempts.
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(users, 0);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn login_checks_the_password_and_does_not_reveal_which_part_was_wrong(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;

    let response = app.try_login("Ada", PASSWORD).await;
    assert!(response.status().is_success());
    let session: lazy_protocol::SessionResponse = response.json().await.unwrap();
    assert_eq!(session.user, ada.user);
    assert_ne!(
        session.token.expose(),
        ada.token,
        "each sign-in gets its own session"
    );

    let (status, wrong_password) = api_error(app.try_login("ada", "not the password").await).await;
    assert_error(status, &wrong_password, ErrorCode::InvalidCredentials);
    let (status, unknown_user) = api_error(app.try_login("nobody", PASSWORD).await).await;
    assert_error(status, &unknown_user, ErrorCode::InvalidCredentials);
    assert_eq!(wrong_password, unknown_user);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn sessions_need_a_valid_unrevoked_unexpired_token(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;

    for header in [
        None,
        Some("Bearer"),
        Some("Bearer nonsense"),
        Some("Basic abc"),
        Some("lzy_abc"),
    ] {
        let mut request = app.http.get(app.url("/v1/bootstrap"));
        if let Some(header) = header {
            request = request.header("authorization", header);
        }
        let (status, error) = api_error(request.send().await.unwrap()).await;
        assert_error(status, &error, ErrorCode::Unauthenticated);
    }
    let unknown = format!("lzy_{}", "0".repeat(64));
    let (status, error) = api_error(app.bootstrap(&unknown).await).await;
    assert_error(status, &error, ErrorCode::Unauthenticated);

    // Expired.
    let second = app
        .try_login("ada", PASSWORD)
        .await
        .json::<lazy_protocol::SessionResponse>()
        .await
        .unwrap();
    assert!(
        app.bootstrap(second.token.expose())
            .await
            .status()
            .is_success()
    );
    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second' WHERE token_hash = sha256($1)")
        .bind(second.token.expose().as_bytes())
        .execute(&app.pool)
        .await
        .unwrap();
    let (status, error) = api_error(app.bootstrap(second.token.expose()).await).await;
    assert_error(status, &error, ErrorCode::Unauthenticated);
    // The first session is unaffected.
    assert!(app.bootstrap(&ada.token).await.status().is_success());
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn logout_revokes_only_that_session(pool: PgPool) {
    let app = App::start(pool).await;
    let first = app.register("ada").await;
    let second = app
        .try_login("ada", PASSWORD)
        .await
        .json::<lazy_protocol::SessionResponse>()
        .await
        .unwrap();

    let response = app
        .http
        .post(app.url("/v1/auth/logout"))
        .bearer_auth(&first.token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 204);

    let (status, error) = api_error(app.bootstrap(&first.token).await).await;
    assert_error(status, &error, ErrorCode::Unauthenticated);
    assert!(
        app.bootstrap(second.token.expose())
            .await
            .status()
            .is_success()
    );
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn registration_can_be_closed(pool: PgPool) {
    let app = App::start_with(
        pool,
        ServerOptions {
            open_registration: false,
            password_cost: PasswordCost::FAST_FOR_TESTS,
            retention: RetentionPolicy::disabled(),
        },
    )
    .await;
    let (status, error) = api_error(app.try_register("ada", PASSWORD).await).await;
    assert_error(status, &error, ErrorCode::Forbidden);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn registration_fails_cleanly_with_no_workspace(pool: PgPool) {
    let server = lazy_server::testing::spawn_test_server(pool.clone()).await;
    let client = reqwest::Client::new();
    let body = serde_json::json!({"username": "ada", "display_name": "Ada", "password": PASSWORD});
    let response = client
        .post(format!("{}/v1/auth/register", server.base_url))
        .json(&body)
        .send()
        .await
        .unwrap();
    let (status, error) = api_error(response).await;
    assert_error(status, &error, ErrorCode::Forbidden);
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(users, 0, "a failed registration leaves no account behind");
    server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn errors_are_json_and_oversized_bodies_are_refused(pool: PgPool) {
    let app = App::start(pool).await;

    let response = app.http.get(app.url("/v1/nope")).send().await.unwrap();
    let (status, error) = api_error(response).await;
    assert_error(status, &error, ErrorCode::NotFound);

    let response = app
        .http
        .post(app.url("/v1/auth/login"))
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    let (status, error) = api_error(response).await;
    assert_error(status, &error, ErrorCode::InvalidRequest);

    let ada = app.register("ada").await;
    let huge = format!(
        r#"{{"operation_id":"{}","document":{{"version":1,"blocks":[]}},"pad":"{}"}}"#,
        OperationId::generate(),
        "x".repeat(300 * 1024)
    );
    let response = app
        .http
        .post(app.url(&lazy_protocol::api::paths::messages(app.general)))
        .bearer_auth(&ada.token)
        .header("content-type", "application/json")
        .body(huge)
        .send()
        .await
        .unwrap();
    let (status, error): (u16, ApiError) = api_error(response).await;
    assert_error(status, &error, ErrorCode::PayloadTooLarge);

    let health: lazy_protocol::HealthResponse = app
        .http
        .get(app.url("/healthz"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health.protocol_version, PROTOCOL_VERSION);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn seeding_is_idempotent_and_safe_under_concurrency(pool: PgPool) {
    let attempts: Vec<_> = (0..8)
        .map(|_| {
            let pool = pool.clone();
            tokio::spawn(async move {
                lazy_server::db::seed_default_workspace(&pool)
                    .await
                    .unwrap()
            })
        })
        .collect();
    let mut created = 0;
    for attempt in attempts {
        if attempt.await.unwrap().is_some() {
            created += 1;
        }
    }
    assert_eq!(
        created, 1,
        "exactly one concurrent seeder creates the workspace"
    );
    let workspaces: i64 = sqlx::query_scalar("SELECT count(*) FROM workspaces")
        .fetch_one(&pool)
        .await
        .unwrap();
    let channels: i64 = sqlx::query_scalar("SELECT count(*) FROM conversations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((workspaces, channels), (1, 1));
    assert!(
        lazy_server::db::seed_default_workspace(&pool)
            .await
            .unwrap()
            .is_none()
    );
}
