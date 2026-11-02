//! Drop-ins: the server hands out join tokens for the call server to people who can read the
//! conversation, and says so plainly when no call server is set up.

mod common;

use common::*;
use lazy_protocol::api::paths;
use lazy_protocol::{ErrorCode, JoinDropInResponse};
use lazy_server::IntegrationOptions;
use lazy_server::drop_ins::DropInOptions;
use sqlx::PgPool;

const SECRET: &str = "0123456789abcdef0123456789abcdef-test";

async fn start(pool: PgPool) -> App {
    App::start_with_integrations(
        pool,
        IntegrationOptions {
            drop_ins: Some(DropInOptions::new("wss://rtc.example.com/", SECRET).unwrap()),
            ..IntegrationOptions::default()
        },
    )
    .await
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[sqlx::test]
async fn readers_get_a_token_for_the_conversations_room(pool: PgPool) {
    let app = start(pool).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;

    let response = app
        .call(
            reqwest::Method::POST,
            &paths::drop_in(app.general),
            &bob,
            None,
        )
        .await;
    assert!(response.status().is_success(), "{}", response.status());
    let joined: JoinDropInResponse = response.json().await.unwrap();
    assert_eq!(joined.url, "wss://rtc.example.com/rtc");
    let claims = rtc_proto::token::verify(&joined.token, SECRET.as_bytes(), now()).unwrap();
    assert_eq!(claims.room, app.general.to_string());
    assert_eq!(claims.identity, bob.user.id.to_string());
    assert_eq!(claims.name, bob.user.display_name.as_str());
    assert!(claims.exp > now() + 60 && claims.exp <= now() + 11 * 60);

    // Someone who cannot read the conversation gets nothing.
    let private = app.create_channel(&alice, "secret", "private").await.id;
    let (status, error) = api_error(
        app.call(reqwest::Method::POST, &paths::drop_in(private), &bob, None)
            .await,
    )
    .await;
    assert_error(status, &error, ErrorCode::NotFound);
    let response = app
        .call(
            reqwest::Method::POST,
            &paths::drop_in(private),
            &alice,
            None,
        )
        .await;
    assert!(response.status().is_success());
}

#[sqlx::test]
async fn without_a_call_server_drop_ins_are_unavailable(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let (status, error) = api_error(
        app.call(
            reqwest::Method::POST,
            &paths::drop_in(app.general),
            &alice,
            None,
        )
        .await,
    )
    .await;
    assert_error(status, &error, ErrorCode::Unavailable);
    assert!(error.message.contains("aren't set up"), "{}", error.message);
}

#[test]
fn settings_are_checked() {
    assert!(DropInOptions::new("https://rtc.example.com", SECRET).is_err());
    assert!(DropInOptions::new("wss://rtc.example.com", "short").is_err());
    let options = DropInOptions::new(" wss://rtc.example.com/rtc ", SECRET).unwrap();
    assert_eq!(options.url, "wss://rtc.example.com/rtc");
    assert!(
        !format!("{options:?}").contains(SECRET),
        "the secret stays out of logs"
    );
}
