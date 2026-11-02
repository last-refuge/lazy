//! Notices (D44): what happens in a conversation is told in it, as a message from whoever did it,
//! and a drop-in the call server reports is announced, kept current, and ended.

mod common;

use common::*;
use lazy_domain::{ConversationId, Notice, UserId};
use lazy_protocol::api::paths;
use lazy_protocol::{Conversation, ConversationResponse, Event, Message};
use lazy_server::IntegrationOptions;
use lazy_server::drop_ins::DropInOptions;
use reqwest::Method;
use rtc_proto::webhook::{self, ReportedParticipant, RoomReport};
use serde_json::json;
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

fn notices(messages: &[Message]) -> Vec<(UserId, Notice, String)> {
    messages
        .iter()
        .filter_map(|m| {
            m.notice
                .clone()
                .map(|notice| (m.author_id, notice, m.document.plain_text()))
        })
        .collect()
}

async fn ok(response: reqwest::Response) -> reqwest::Response {
    assert!(
        response.status().is_success(),
        "status {}",
        response.status()
    );
    response
}

async fn conversation(response: reqwest::Response) -> Conversation {
    ok(response)
        .await
        .json::<ConversationResponse>()
        .await
        .unwrap()
        .conversation
}

#[sqlx::test]
async fn what_happens_in_a_channel_is_told_in_it_quietly(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;

    // Joining the workspace is told in the default channel.
    let general = app.history_ok(&alice, app.general, &[]).await.messages;
    let said = notices(&general);
    assert_eq!(said.len(), 2, "{said:?}");
    assert_eq!(said[1].0, bob.user.id);
    assert_eq!(said[1].1, Notice::JoinedWorkspace);
    assert_eq!(said[1].2, "joined the workspace");

    let team = app.create_channel(&alice, "team", "public").await.id;
    ok(app.act(&bob, team, "join").await).await;
    ok(app.act(&bob, team, "leave").await).await;
    conversation(app.add_members(&alice, team, &[&bob]).await).await;
    ok(app.remove_member(&alice, team, &bob).await).await;
    conversation(
        app.update(&alice, team, json!({"name": "crew", "topic": "Ship it"}))
            .await,
    )
    .await;
    conversation(app.update(&alice, team, json!({"topic": ""})).await).await;
    conversation(app.act(&alice, team, "archive").await).await;

    let history = app.history_ok(&alice, team, &[]).await.messages;
    let said = notices(&history);
    let expected = [
        (bob.user.id, Notice::Joined, "joined #team"),
        (bob.user.id, Notice::Left, "left #team"),
        (
            alice.user.id,
            Notice::Added {
                users: vec![bob.user.id],
            },
            "added User bob to #team",
        ),
        (
            alice.user.id,
            Notice::Removed { user: bob.user.id },
            "removed User bob from #team",
        ),
        (
            alice.user.id,
            Notice::Renamed {
                from: "team".into(),
                to: "crew".into(),
            },
            "renamed the channel from #team to #crew",
        ),
        (
            alice.user.id,
            Notice::TopicChanged {
                topic: Some("Ship it".into()),
            },
            "set the topic: Ship it",
        ),
        (
            alice.user.id,
            Notice::TopicChanged { topic: None },
            "cleared the topic",
        ),
        (alice.user.id, Notice::Archived, "archived #crew"),
    ];
    assert_eq!(said.len(), expected.len(), "{said:?}");
    for ((author, notice, words), (want_author, want_notice, want_words)) in
        said.iter().zip(expected)
    {
        assert_eq!(*author, want_author);
        assert_eq!(*notice, want_notice);
        assert_eq!(words, want_words);
    }

    // Quiet: nobody has them as unread (Alice's own are read; Bob's joining is not unread).
    assert_eq!(app.state_of(&alice, app.general).await.unread_count, 0);

    // Nobody edits a notice, not even its author.
    let joined = history.iter().find(|m| m.notice.is_some()).unwrap();
    let (status, _) = api_error(
        app.edit_raw(
            &alice,
            joined.id,
            lazy_domain::OperationId::generate(),
            joined.revision,
            "something else",
        )
        .await,
    )
    .await;
    assert_eq!(status, 403);
    // And search does not find them.
    let found = ok(app
        .call(
            Method::GET,
            &format!("{}?q=joined", paths::search(app.workspace_id)),
            &alice,
            None,
        )
        .await)
    .await
    .json::<serde_json::Value>()
    .await
    .unwrap();
    assert_eq!(found["hits"].as_array().map(Vec::len), Some(0), "{found}");
}

fn signed(conversation: ConversationId, people: &[&Account], sent_at_ms: u64) -> (Vec<u8>, String) {
    let body = serde_json::to_vec(&RoomReport {
        room: conversation.to_string(),
        participants: people
            .iter()
            .map(|account| ReportedParticipant {
                identity: account.user.id.to_string(),
                name: account.user.display_name.as_str().to_owned(),
            })
            .collect(),
        sent_at_ms,
    })
    .unwrap();
    let signature = webhook::sign(&body, SECRET.as_bytes()).unwrap();
    (body, signature)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

async fn report(app: &App, conversation: ConversationId, people: &[&Account], at: u64) -> u16 {
    let (body, signature) = signed(conversation, people, at);
    app.http
        .post(app.url(paths::RTC_REPORT_ROUTE))
        .header(webhook::SIGNATURE_HEADER, signature)
        .body(body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[sqlx::test]
async fn a_drop_in_in_a_direct_conversation_is_announced_kept_current_and_ended(pool: PgPool) {
    let app = start(pool).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let dm = app.open(&alice, &[&bob]).await.id;
    let mut bob_ws = app.connect_ws(&bob).await;
    bob_ws.subscribe(app.workspace_id, app.head().await).await;
    bob_ws.replay().await;

    // Alice drops in: Bob is told, with a notice that counts as unread (and so can notify).
    let t = now_ms();
    assert_eq!(report(&app, dm, &[&alice], t).await, 204);
    let state = app.state_of(&bob, dm).await;
    assert_eq!(state.unread_count, 1, "a drop-in starting is unread");
    let history = app.history_ok(&bob, dm, &[]).await.messages;
    let notice = history.last().unwrap();
    assert_eq!(notice.author_id, alice.user.id);
    assert_eq!(notice.document.plain_text(), "started a drop-in");
    let started = notice.notice.as_ref().and_then(Notice::as_drop_in).unwrap();
    assert!(started.is_live());
    assert_eq!(started.participants, vec![alice.user.id]);
    let live = app
        .conversations_of(&bob)
        .await
        .into_iter()
        .find(|c| c.id == dm)
        .unwrap()
        .drop_in
        .expect("the conversation shows the drop-in");
    assert_eq!(live.message_id, notice.id);
    assert_eq!(live.participants, vec![alice.user.id]);

    // A repeat (a heartbeat) changes nothing anyone sees; an older report is ignored.
    let head = app.head().await;
    assert_eq!(report(&app, dm, &[&alice], t + 1).await, 204);
    assert_eq!(report(&app, dm, &[], t - 1).await, 204);
    assert_eq!(app.head().await, head);

    // Bob joins: the notice and the conversation say so.
    assert_eq!(report(&app, dm, &[&alice, &bob], t + 2).await, 204);
    let updated = app.get_message(&bob, notice.id).await;
    let now = updated
        .notice
        .as_ref()
        .and_then(Notice::as_drop_in)
        .unwrap();
    assert_eq!(now.participants, vec![alice.user.id, bob.user.id]);
    assert_eq!(updated.edited_at, None, "an update is not an edit");
    assert!(updated.revision > notice.revision);

    // Everyone leaves: it ended, and says for how long.
    assert_eq!(report(&app, dm, &[], t + 3).await, 204);
    let ended = app.get_message(&bob, notice.id).await;
    let drop_in = ended.notice.as_ref().and_then(Notice::as_drop_in).unwrap();
    assert!(!drop_in.is_live());
    assert!(drop_in.participants.is_empty());
    assert_eq!(drop_in.joined, vec![alice.user.id, bob.user.id]);
    assert!(
        ended
            .document
            .plain_text()
            .starts_with("started a drop-in, which ended after"),
        "{}",
        ended.document.plain_text()
    );
    let dm_now = app
        .conversations_of(&bob)
        .await
        .into_iter()
        .find(|c| c.id == dm)
        .unwrap();
    assert_eq!(dm_now.drop_in, None);

    // Bob heard all of it, in order: the notice, the conversation, then the updates.
    let marker = app.register("marker").await;
    let events = bob_ws.events_until_marker(marker.user.id).await;
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match &e.event {
            Event::MessageCreated { .. } => "created",
            Event::MessageEdited { .. } => "edited",
            Event::ConversationUpdated { conversation } if conversation.drop_in.is_some() => "live",
            Event::ConversationUpdated { .. } => "none",
            _ => "other",
        })
        .filter(|kind| *kind != "other")
        .collect();
    assert_eq!(
        kinds,
        ["created", "live", "live", "edited", "none", "edited"]
    );

    // A new report afterwards starts a new drop-in, with its own notice.
    assert_eq!(report(&app, dm, &[&bob], t + 4).await, 204);
    let history = app.history_ok(&alice, dm, &[]).await.messages;
    assert_eq!(
        history
            .iter()
            .filter(|m| m.notice.as_ref().and_then(Notice::as_drop_in).is_some())
            .count(),
        2
    );
    assert_eq!(history.last().unwrap().author_id, bob.user.id);
}

#[sqlx::test]
async fn reports_must_be_signed_fresh_and_about_us(pool: PgPool) {
    let app = start(pool).await;
    let alice = app.register("alice").await;
    let head = app.head().await;

    // Forged, or too old.
    let (body, _) = signed(app.general, &[&alice], now_ms());
    let forged = app
        .http
        .post(app.url(paths::RTC_REPORT_ROUTE))
        .header(webhook::SIGNATURE_HEADER, "v1=AAAA")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status().as_u16(), 401);
    assert_eq!(
        report(&app, app.general, &[&alice], now_ms() - 3_600_000).await,
        401
    );
    // A room that is no conversation, or people who are nobody, change nothing.
    let (body, signature) = {
        let body = serde_json::to_vec(&RoomReport {
            room: "not-a-conversation".into(),
            participants: vec![ReportedParticipant {
                identity: "someone".into(),
                name: "Someone".into(),
            }],
            sent_at_ms: now_ms(),
        })
        .unwrap();
        let signature = webhook::sign(&body, SECRET.as_bytes()).unwrap();
        (body, signature)
    };
    let response = app
        .http
        .post(app.url(paths::RTC_REPORT_ROUTE))
        .header(webhook::SIGNATURE_HEADER, signature)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 204);
    assert_eq!(app.head().await, head);

    // Without a call server there is nothing to report to.
    let plain = App::restart(
        &app.pool,
        lazy_server::ServerOptions {
            open_registration: true,
            open_workspace_creation: true,
            password_cost: lazy_server::PasswordCost::FAST_FOR_TESTS,
            retention: lazy_server::RetentionPolicy::disabled(),
        },
        app.workspace_id,
        app.general,
    )
    .await;
    let (body, signature) = signed(app.general, &[&alice], now_ms());
    let response = plain
        .http
        .post(plain.url(paths::RTC_REPORT_ROUTE))
        .header(webhook::SIGNATURE_HEADER, signature)
        .body(body)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_client_error() || response.status().is_server_error());
}

#[sqlx::test]
async fn a_drop_in_nobody_reports_on_any_more_ends(pool: PgPool) {
    let app = start(pool).await;
    let alice = app.register("alice").await;
    assert_eq!(report(&app, app.general, &[&alice], now_ms()).await, 204);
    assert!(
        app.conversations_of(&alice)
            .await
            .iter()
            .any(|c| c.id == app.general && c.drop_in.is_some())
    );
    // Not yet silent long enough.
    let ended = lazy_server::drop_ins::end_silent(&app.pool, std::time::Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(ended, 0);
    // Silent: the call server restarted, or cannot reach us.
    let ended = lazy_server::drop_ins::end_silent(&app.pool, std::time::Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(ended, 1);
    let general = app
        .conversations_of(&alice)
        .await
        .into_iter()
        .find(|c| c.id == app.general)
        .unwrap();
    assert_eq!(general.drop_in, None);
    let history = app.history_ok(&alice, app.general, &[]).await.messages;
    let drop_in = history
        .iter()
        .rev()
        .find_map(|m| m.notice.as_ref().and_then(Notice::as_drop_in))
        .unwrap();
    assert!(!drop_in.is_live());
}
