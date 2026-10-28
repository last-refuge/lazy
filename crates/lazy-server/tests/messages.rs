mod common;

use std::collections::HashSet;

use common::*;
use lazy_domain::{ConversationId, MessageDocument, OperationId};
use lazy_protocol::{ErrorCode, OperationStatus, SendMessageRequest, SendMessageResponse};
use sqlx::PgPool;

#[sqlx::test]
async fn sends_are_acknowledged_with_what_was_committed(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;

    let operation = OperationId::generate();
    let response = app
        .send_raw(&ada, app.general, operation, "hello\nworld")
        .await;
    assert!(response.status().is_success());
    let SendMessageResponse { message } = response.json().await.unwrap();
    assert_eq!(message.conversation_id, app.general);
    assert_eq!(message.author_id, ada.user.id);
    assert_eq!(message.revision, 1);
    assert_eq!(message.document.plain_text(), "hello\nworld");
    // Registration took positions 1 and 2.
    assert_eq!(message.seq.get(), 3);
    assert_eq!(app.head().await, message.seq);

    let stored: (String, i64) =
        sqlx::query_as("SELECT plain_text, seq FROM messages WHERE id = $1")
            .bind(message.id.as_uuid())
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(stored, ("hello\nworld".to_owned(), 3));
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn repeating_an_operation_returns_the_original_result_without_a_second_message(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let operation = OperationId::generate();

    let first: SendMessageResponse = app
        .send_raw(&ada, app.general, operation, "once")
        .await
        .json()
        .await
        .unwrap();
    let again: SendMessageResponse = app
        .send_raw(&ada, app.general, operation, "once")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(first, again);

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    // A duplicate must not consume a log position either.
    assert_eq!(app.head().await, first.message.seq);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn reusing_an_operation_id_for_different_content_is_a_conflict(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let other = app.create_conversation("other", "public", false).await;
    app.add_to_conversation(other, &ada).await;
    let operation = OperationId::generate();
    app.send_raw(&ada, app.general, operation, "original").await;

    let (status, error) =
        api_error(app.send_raw(&ada, app.general, operation, "changed").await).await;
    assert_error(status, &error, ErrorCode::Conflict);
    let (status, error) = api_error(app.send_raw(&ada, other, operation, "original").await).await;
    assert_error(status, &error, ErrorCode::Conflict);

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn operation_ids_are_scoped_to_the_sender(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;
    let operation = OperationId::generate();

    let a = app.send_raw(&ada, app.general, operation, "from ada").await;
    let g = app
        .send_raw(&grace, app.general, operation, "from grace")
        .await;
    assert!(a.status().is_success() && g.status().is_success());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(count, 2);

    // Grace cannot learn the outcome of an operation she did not submit.
    let third = OperationId::generate();
    app.send_raw(&ada, app.general, third, "private outcome")
        .await;
    assert_eq!(
        app.operation(&grace, third).await,
        OperationStatus::NotFound
    );
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn simultaneous_duplicates_commit_exactly_one_message(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let operation = OperationId::generate();

    let attempts: Vec<_> = (0..16)
        .map(|_| {
            let http = app.http.clone();
            let url = app.url(&lazy_protocol::api::paths::messages(app.general));
            let token = ada.token.clone();
            tokio::spawn(async move {
                let body = SendMessageRequest {
                    operation_id: operation,
                    document: MessageDocument::from_plain_text("racing").unwrap(),
                };
                http.post(url)
                    .bearer_auth(token)
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
            })
        })
        .collect();
    let mut ids = HashSet::new();
    for attempt in attempts {
        let response = attempt.await.unwrap();
        assert!(response.status().is_success(), "{}", response.status());
        let SendMessageResponse { message } = response.json().await.unwrap();
        ids.insert(message.id);
    }
    assert_eq!(ids.len(), 1, "every duplicate gets the same message back");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_lost_acknowledgment_is_recoverable_from_the_operation_status(pool: PgPool) {
    let app = App::start(pool.clone()).await;
    let ada = app.register("ada").await;
    let operation = OperationId::generate();

    // The server commits, but the response never reaches the client: call the service directly
    // and discard what it returns.
    let request = SendMessageRequest {
        operation_id: operation,
        document: MessageDocument::from_plain_text("did this arrive?").unwrap(),
    };
    let committed = lazy_server::messages::send_message(&pool, ada.user.id, app.general, request)
        .await
        .unwrap();

    // The client asks before doing anything else.
    match app.operation(&ada, operation).await {
        OperationStatus::Committed { message } => assert_eq!(message, committed),
        other => panic!("expected committed, got {other:?}"),
    }
    // Retrying the same operation yields the same message and creates nothing new.
    let retried: SendMessageResponse = app
        .send_raw(&ada, app.general, operation, "did this arrive?")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(retried.message, committed);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    // An operation that never reached the server is reported as unknown to it.
    assert_eq!(
        app.operation(&ada, OperationId::generate()).await,
        OperationStatus::NotFound
    );
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn authorization_is_enforced_for_every_conversation_state(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;

    let secret = app.create_conversation("secret", "private", false).await;
    app.add_to_conversation(secret, &ada).await;
    let archived = app.create_conversation("old", "public", true).await;
    app.add_to_conversation(archived, &ada).await;
    let unjoined = app.create_conversation("unjoined", "public", false).await;

    // A private channel does not exist as far as non-members can tell.
    let (status, error) = api_error(
        app.send_raw(&grace, secret, OperationId::generate(), "hi")
            .await,
    )
    .await;
    assert_error(status, &error, ErrorCode::NotFound);
    let (status, error) = api_error(app.history(&grace, secret, &[]).await).await;
    assert_error(status, &error, ErrorCode::NotFound);
    assert!(
        app.send_raw(&ada, secret, OperationId::generate(), "members only")
            .await
            .status()
            .is_success()
    );

    // Archived channels stay readable but cannot be posted to.
    let (status, error) = api_error(
        app.send_raw(&ada, archived, OperationId::generate(), "hi")
            .await,
    )
    .await;
    assert_error(status, &error, ErrorCode::Forbidden);
    assert!(app.history(&ada, archived, &[]).await.status().is_success());

    // Public channels can be read without joining, but posting needs membership.
    assert!(
        app.history(&grace, unjoined, &[])
            .await
            .status()
            .is_success()
    );
    let (status, error) = api_error(
        app.send_raw(&grace, unjoined, OperationId::generate(), "hi")
            .await,
    )
    .await;
    assert_error(status, &error, ErrorCode::Forbidden);

    // Unknown conversations and unauthenticated callers.
    let missing = ConversationId::generate();
    let (status, error) = api_error(app.history(&ada, missing, &[]).await).await;
    assert_error(status, &error, ErrorCode::NotFound);
    let response = app
        .http
        .get(app.url(&lazy_protocol::api::paths::messages(app.general)))
        .send()
        .await
        .unwrap();
    let (status, error) = api_error(response).await;
    assert_error(status, &error, ErrorCode::Unauthenticated);

    // Rejected sends leave no receipt behind: the operation id is still usable.
    let operation = OperationId::generate();
    let (status, _) = api_error(app.send_raw(&grace, unjoined, operation, "hi").await).await;
    assert_eq!(status, 403);
    assert_eq!(
        app.operation(&grace, operation).await,
        OperationStatus::NotFound
    );
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn people_outside_the_workspace_see_nothing(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;
    app.send(&ada, app.general, "hello").await;

    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
        .bind(grace.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    let (status, error) = api_error(app.history(&grace, app.general, &[]).await).await;
    assert_error(status, &error, ErrorCode::NotFound);
    let (status, error) = api_error(
        app.send_raw(&grace, app.general, OperationId::generate(), "hi")
            .await,
    )
    .await;
    assert_error(status, &error, ErrorCode::NotFound);
    assert!(app.bootstrap_ok(&grace).await.workspaces.is_empty());
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn history_pages_are_ordered_by_position_and_do_not_overlap(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;

    let mut sent = Vec::new();
    for index in 0..12 {
        let sender = if index % 2 == 0 { &ada } else { &grace };
        sent.push(
            app.send(sender, app.general, &format!("message {index}"))
                .await,
        );
    }
    assert!(sent.windows(2).all(|pair| pair[0].seq < pair[1].seq));

    // The newest page comes back oldest-first.
    let newest = app
        .history_ok(&ada, app.general, &[("limit", "5".into())])
        .await;
    assert_eq!(newest.messages, sent[7..].to_vec());
    assert!(newest.has_more);

    // Walk back to the beginning.
    let mut before = newest.messages[0].seq;
    let middle = app
        .history_ok(
            &ada,
            app.general,
            &[("limit", "5".into()), ("before", before.get().to_string())],
        )
        .await;
    assert_eq!(middle.messages, sent[2..7].to_vec());
    assert!(middle.has_more);
    before = middle.messages[0].seq;
    let oldest = app
        .history_ok(
            &ada,
            app.general,
            &[("limit", "5".into()), ("before", before.get().to_string())],
        )
        .await;
    assert_eq!(oldest.messages, sent[..2].to_vec());
    assert!(!oldest.has_more);

    // Defaults and clamping.
    let all = app.history_ok(&ada, app.general, &[]).await;
    assert_eq!(all.messages, sent);
    assert!(!all.has_more);
    let clamped = app
        .history_ok(&ada, app.general, &[("limit", "0".into())])
        .await;
    assert_eq!(clamped.messages.len(), 1);
    let (status, error) = api_error(
        app.history(&ada, app.general, &[("limit", "many".into())])
            .await,
    )
    .await;
    assert_error(status, &error, ErrorCode::InvalidRequest);
    let (status, error) = api_error(
        app.history(&ada, app.general, &[("before", "-3".into())])
            .await,
    )
    .await;
    assert_error(status, &error, ErrorCode::InvalidRequest);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn concurrent_senders_get_distinct_gapless_positions(pool: PgPool) {
    let app = App::start(pool).await;
    let mut accounts = Vec::new();
    for name in ["user1", "user2", "user3", "user4"] {
        accounts.push(app.register(name).await);
    }
    let first_position = app.head().await;

    let mut tasks = Vec::new();
    for (index, account) in accounts.iter().enumerate() {
        for n in 0..10 {
            let http = app.http.clone();
            let url = app.url(&lazy_protocol::api::paths::messages(app.general));
            let token = account.token.clone();
            tasks.push(tokio::spawn(async move {
                let body = SendMessageRequest {
                    operation_id: OperationId::generate(),
                    document: MessageDocument::from_plain_text(&format!("{index}-{n}")).unwrap(),
                };
                let response = http
                    .post(url)
                    .bearer_auth(token)
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                assert!(response.status().is_success());
                response
                    .json::<SendMessageResponse>()
                    .await
                    .unwrap()
                    .message
                    .seq
            }));
        }
    }
    let mut positions = Vec::new();
    for task in tasks {
        positions.push(task.await.unwrap().get());
    }
    positions.sort_unstable();
    let expected: Vec<i64> = (first_position.get() + 1..=first_position.get() + 40).collect();
    assert_eq!(positions, expected);
    assert_eq!(app.log_positions_after(first_position).await, expected);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn bootstrap_snapshots_always_match_their_head(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;

    // Register people concurrently while ada keeps taking snapshots.
    let registrations: Vec<_> = (0..12)
        .map(|index| {
            let http = app.http.clone();
            let url = app.url("/v1/auth/register");
            tokio::spawn(async move {
                let body = serde_json::json!({
                    "username": format!("person{index}"),
                    "display_name": format!("Person {index}"),
                    "password": PASSWORD,
                });
                let response = http.post(url).json(&body).send().await.unwrap();
                assert!(response.status().is_success());
            })
        })
        .collect();

    let mut snapshots = 0;
    loop {
        let snapshot = app.bootstrap_ok(&ada).await;
        let workspace = &snapshot.workspaces[0];
        // Each registration is exactly one event, so a consistent snapshot has as many members as
        // `member_joined` events at or before its head.
        let joined: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM events WHERE workspace_id = $1 AND kind = 'member_joined' AND seq <= $2",
        )
        .bind(app.workspace_id.as_uuid())
        .bind(workspace.head.get())
        .fetch_one(&app.pool)
        .await
        .unwrap();
        assert_eq!(
            workspace.members.len() as i64,
            joined,
            "snapshot at head {}",
            workspace.head
        );
        snapshots += 1;
        if workspace.members.len() == 13 {
            break;
        }
        assert!(snapshots < 2000, "registrations never completed");
    }
    for registration in registrations {
        registration.await.unwrap();
    }
    app.server.shutdown().await.unwrap();
}
