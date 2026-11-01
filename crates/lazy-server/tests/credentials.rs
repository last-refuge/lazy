//! Encrypted integration credentials (D31): stored and read back, nothing readable in the
//! database, rows that cannot be swapped, revocation, replacement, a server with no key, and a key
//! rotation with `rewrap_all`.

use lazy_domain::WorkspaceId;
use lazy_protocol::ErrorCode;
use lazy_server::credentials::{self, CredentialId, CredentialKeys, Secret};
use lazy_server::db;
use sqlx::PgPool;

const TOKEN: &str = "ghs_live_0123456789abcdefTOKEN";

async fn workspace(pool: &PgPool) -> WorkspaceId {
    db::seed_default_workspace_with(pool, Default::default())
        .await
        .unwrap()
        .expect("a fresh database gets a workspace")
        .workspace_id
}

fn code_of(error: lazy_server::AppError) -> Option<ErrorCode> {
    match error {
        lazy_server::AppError::Api(api) => Some(api.code),
        _ => None,
    }
}

#[sqlx::test]
async fn a_credential_is_stored_sealed_and_read_back(pool: PgPool) {
    let ws = workspace(&pool).await;
    let keys = CredentialKeys::from_bytes([1; 32], &[]);
    let mut conn = pool.acquire().await.unwrap();
    let id = credentials::store(
        &mut conn,
        &keys,
        ws,
        "github.token",
        &Secret::from_text(TOKEN),
    )
    .await
    .unwrap();
    let back = credentials::load(&mut conn, &keys, id).await.unwrap();
    assert_eq!(back.expose_text(), Some(TOKEN));
    assert_eq!(format!("{back:?}"), "Secret(<redacted>)");

    // Nothing in the row is the secret.
    let row: String = sqlx::query_scalar(
        "SELECT row_to_json(c)::text FROM integration_credentials c WHERE id = $1",
    )
    .bind(id.0)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert!(!row.contains(TOKEN) && !row.contains("ghs_live"), "{row}");
    let key_id: String =
        sqlx::query_scalar("SELECT key_id FROM integration_credentials WHERE id = $1")
            .bind(id.0)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(Some(key_id.as_str()), keys.current_id());

    // Another key does not open it, and an unknown id is not found.
    let other = CredentialKeys::from_bytes([2; 32], &[]);
    assert_eq!(
        code_of(credentials::load(&mut conn, &other, id).await.unwrap_err()),
        Some(ErrorCode::Unavailable)
    );
    assert_eq!(
        code_of(
            credentials::load(&mut conn, &keys, CredentialId(uuid::Uuid::now_v7()))
                .await
                .unwrap_err()
        ),
        Some(ErrorCode::NotFound)
    );
}

#[sqlx::test]
async fn a_ciphertext_moved_to_another_row_does_not_open(pool: PgPool) {
    let ws = workspace(&pool).await;
    let keys = CredentialKeys::from_bytes([1; 32], &[]);
    let mut conn = pool.acquire().await.unwrap();
    let token = credentials::store(
        &mut conn,
        &keys,
        ws,
        "github.token",
        &Secret::from_text(TOKEN),
    )
    .await
    .unwrap();
    let decoy = credentials::store(
        &mut conn,
        &keys,
        ws,
        "github.webhook_secret",
        &Secret::from_text("x"),
    )
    .await
    .unwrap();
    // Someone with write access to the database copies the token's ciphertext into another row.
    sqlx::query(
        "UPDATE integration_credentials d SET nonce = t.nonce, ciphertext = t.ciphertext \
         FROM integration_credentials t WHERE d.id = $1 AND t.id = $2",
    )
    .bind(decoy.0)
    .bind(token.0)
    .execute(&mut *conn)
    .await
    .unwrap();
    let error = credentials::load(&mut conn, &keys, decoy)
        .await
        .unwrap_err();
    assert!(
        matches!(error, lazy_server::AppError::Internal(_)),
        "{error:?}"
    );
    assert!(!format!("{error:?}").contains(TOKEN));
}

#[sqlx::test]
async fn revoking_erases_the_secret_and_replacing_reseals_it(pool: PgPool) {
    let ws = workspace(&pool).await;
    let keys = CredentialKeys::from_bytes([1; 32], &[]);
    let mut conn = pool.acquire().await.unwrap();
    let id = credentials::store(
        &mut conn,
        &keys,
        ws,
        "github.token",
        &Secret::from_text("old"),
    )
    .await
    .unwrap();
    credentials::replace(&mut conn, &keys, id, &Secret::from_text("new"))
        .await
        .unwrap();
    assert_eq!(
        credentials::load(&mut conn, &keys, id)
            .await
            .unwrap()
            .expose_text(),
        Some("new")
    );
    credentials::revoke(&mut conn, id).await.unwrap();
    credentials::revoke(&mut conn, id).await.unwrap();
    assert_eq!(
        code_of(credentials::load(&mut conn, &keys, id).await.unwrap_err()),
        Some(ErrorCode::NotFound)
    );
    let erased: (Option<Vec<u8>>, bool) = sqlx::query_as(
        "SELECT ciphertext, revoked_at IS NOT NULL FROM integration_credentials WHERE id = $1",
    )
    .bind(id.0)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(erased, (None, true), "the row stays, the secret does not");
    assert_eq!(
        code_of(
            credentials::replace(&mut conn, &keys, id, &Secret::from_text("again"))
                .await
                .unwrap_err()
        ),
        Some(ErrorCode::NotFound)
    );
}

#[sqlx::test]
async fn without_a_key_credentials_are_unavailable(pool: PgPool) {
    let ws = workspace(&pool).await;
    let none = CredentialKeys::none();
    let mut conn = pool.acquire().await.unwrap();
    let error = credentials::store(
        &mut conn,
        &none,
        ws,
        "github.token",
        &Secret::from_text(TOKEN),
    )
    .await
    .unwrap_err();
    assert_eq!(code_of(error), Some(ErrorCode::Unavailable));
    let error = credentials::load(&mut conn, &none, CredentialId(uuid::Uuid::now_v7()))
        .await
        .unwrap_err();
    assert_eq!(code_of(error), Some(ErrorCode::Unavailable));
}

#[sqlx::test]
async fn a_rotation_reseals_everything_under_the_new_key(pool: PgPool) {
    let ws = workspace(&pool).await;
    let old = CredentialKeys::from_bytes([1; 32], &[]);
    let mut conn = pool.acquire().await.unwrap();
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(
            credentials::store(
                &mut conn,
                &old,
                ws,
                "github.token",
                &Secret::from_text(&format!("t{i}")),
            )
            .await
            .unwrap(),
        );
    }
    let revoked = ids.pop().unwrap();
    credentials::revoke(&mut conn, revoked).await.unwrap();
    drop(conn);

    // During the rotation: the new key is current, the old one only opens.
    let rotating = CredentialKeys::from_bytes([9; 32], &[[1; 32]]);
    let mut conn = pool.acquire().await.unwrap();
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            credentials::load(&mut conn, &rotating, *id)
                .await
                .unwrap()
                .expose_text(),
            Some(format!("t{i}").as_str()),
            "old rows still open"
        );
    }
    drop(conn);
    let report = credentials::rewrap_all(&pool, &rotating).await.unwrap();
    assert_eq!(
        (report.rewrapped, report.current, report.missing_key),
        (4, 0, 0)
    );
    let again = credentials::rewrap_all(&pool, &rotating).await.unwrap();
    assert_eq!(
        (again.rewrapped, again.current),
        (0, 4),
        "running it again changes nothing"
    );

    // After it: the old key can go.
    let rotated = CredentialKeys::from_bytes([9; 32], &[]);
    let mut conn = pool.acquire().await.unwrap();
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            credentials::load(&mut conn, &rotated, *id)
                .await
                .unwrap()
                .expose_text(),
            Some(format!("t{i}").as_str())
        );
    }
    // A key that was dropped too early leaves rows it cannot open, and says so.
    let lost = CredentialKeys::from_bytes([5; 32], &[]);
    let report = credentials::rewrap_all(&pool, &lost).await.unwrap();
    assert_eq!(report.missing_key, 4);
}
