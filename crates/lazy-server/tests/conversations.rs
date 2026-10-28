//! Creating channels and opening direct and group conversations: results, idempotency, and the
//! concurrency guarantees (exactly one winner, exactly one `ConversationCreated`).

mod common;

use common::*;
use lazy_domain::{ChannelVisibility, ConversationKind, OperationId};
use lazy_protocol::api::paths;
use lazy_protocol::{
    Conversation, ConversationResponse, ErrorCode, Event, EventEnvelope, OperationStatus,
};
use reqwest::Method;
use serde_json::json;
use sqlx::PgPool;

pub fn names(conversations: &[Conversation]) -> Vec<&str> {
    conversations
        .iter()
        .filter_map(|c| c.name.as_ref().map(|n| n.as_str()))
        .collect()
}

pub fn label(envelope: &EventEnvelope) -> &'static str {
    match envelope.event {
        Event::MessageCreated { .. } => "message_created",
        Event::MemberJoined { .. } => "member_joined",
        Event::ConversationCreated { .. } => "conversation_created",
        Event::ConversationUpdated { .. } => "conversation_updated",
        Event::MemberAdded { .. } => "member_added",
        Event::MemberRemoved { .. } => "member_removed",
        Event::UserStateChanged { .. } => "user_state_changed",
    }
}

/// A socket subscribed at the current head, with the replay already consumed.
pub async fn subscribed(app: &App, who: &Account) -> Ws {
    let mut ws = app.connect_ws(who).await;
    ws.subscribe(app.workspace_id, app.head().await).await;
    ws.replay().await;
    ws
}

async fn conversation_of(response: reqwest::Response) -> Conversation {
    assert!(
        response.status().is_success(),
        "status {}",
        response.status()
    );
    response
        .json::<ConversationResponse>()
        .await
        .expect("conversation json")
        .conversation
}

async fn denied(response: reqwest::Response, code: ErrorCode) {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
}

async fn event_count(app: &App, kind: &str, after: lazy_domain::EventSeq) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM events WHERE kind = $1 AND seq > $2")
        .bind(kind)
        .bind(after.get())
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn new_accounts_join_only_the_default_channel(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let random = app.create_channel(&ada, "random", "public").await;
    let mut ada_ws = subscribed(&app, &ada).await;

    let grace = app.register("grace").await;
    assert_eq!(names(&app.conversations_of(&grace).await), vec!["general"]);
    // Other public channels are found by browsing, and joined explicitly.
    let browse: lazy_protocol::BrowseChannelsResponse = app
        .call(
            Method::GET,
            &paths::browse_channels(app.workspace_id),
            &grace,
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    let joined: Vec<(&str, bool)> = browse
        .channels
        .iter()
        .map(|c| (c.conversation.name.as_ref().unwrap().as_str(), c.joined))
        .collect();
    assert_eq!(joined, vec![("general", true), ("random", false)]);

    // Ada learns of grace joining the workspace, then #general, with the count as of that moment.
    let marker = app.register("marker").await;
    let events = ada_ws.events_until_marker(marker.user.id).await;
    assert_eq!(
        events.iter().map(label).collect::<Vec<_>>(),
        vec!["member_joined", "member_added"]
    );
    match &events[1].event {
        Event::MemberAdded {
            conversation,
            user_id,
        } => {
            assert_eq!(*user_id, grace.user.id);
            assert_eq!(conversation.id, app.general);
            assert_eq!(conversation.member_count, 2);
            assert!(conversation.is_default);
        }
        other => panic!("unexpected event {other:?}"),
    }
    assert_ne!(random.id, app.general);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn creating_a_channel_makes_the_creator_its_first_member(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;
    let mut ada_ws = subscribed(&app, &ada).await;
    let mut grace_ws = subscribed(&app, &grace).await;

    let operation = OperationId::generate();
    let body = json!({
        "operation_id": operation, "name": "design", "visibility": "private",
        "topic": "Pixels", "description": "Where design happens.\nBe kind.",
    });
    let path = paths::channels(app.workspace_id);
    let channel = conversation_of(
        app.call(Method::POST, &path, &grace, Some(body.clone()))
            .await,
    )
    .await;
    assert_eq!(channel.kind, ConversationKind::Channel);
    assert_eq!(channel.name.as_ref().unwrap().as_str(), "design");
    assert_eq!(channel.topic.as_deref(), Some("Pixels"));
    assert_eq!(
        channel.description.as_deref(),
        Some("Where design happens.\nBe kind.")
    );
    assert_eq!(channel.visibility, ChannelVisibility::Private);
    assert!(!channel.archived && !channel.is_default);
    assert_eq!(channel.created_by, Some(grace.user.id));
    assert!(channel.participants.is_empty());
    assert_eq!(channel.member_count, 1);

    // The creator has it in the sidebar, starting with nothing unread; others do not.
    assert!(names(&app.conversations_of(&grace).await).contains(&"design"));
    assert!(!names(&app.conversations_of(&ada).await).contains(&"design"));
    let state = app.state_of(&grace, channel.id).await;
    assert_eq!(
        (state.favorite, state.last_read.get(), state.unread_count),
        (false, 0, 0)
    );

    // A repeat with the same operation id returns the same channel without a second event.
    let head = app.head().await;
    let again = conversation_of(app.call(Method::POST, &path, &grace, Some(body)).await).await;
    assert_eq!(again, channel);
    assert_eq!(app.head().await, head);

    // The same id for something else is a conflict; the name is taken for everyone.
    let other = json!({"operation_id": operation, "name": "other", "visibility": "private", "topic": null, "description": null});
    denied(
        app.call(Method::POST, &path, &grace, Some(other)).await,
        ErrorCode::Conflict,
    )
    .await;
    denied(
        app.create_channel_raw(&ada, "design", "public", OperationId::generate())
            .await,
        ErrorCode::NameTaken,
    )
    .await;
    denied(
        app.create_channel_raw(&ada, "general", "public", OperationId::generate())
            .await,
        ErrorCode::NameTaken,
    )
    .await;

    // A lost response is reconciled through the operation status.
    match app.operation(&grace, operation).await {
        OperationStatus::ConversationCommitted { conversation } => {
            assert_eq!(conversation, channel)
        }
        other => panic!("unexpected status {other:?}"),
    }
    assert_eq!(
        app.operation(&ada, operation).await,
        OperationStatus::NotFound,
        "operation ids are per person"
    );

    // Only the creator hears about it; being private, nobody else does.
    let marker = app.register("marker").await;
    let hers = grace_ws.events_until_marker(marker.user.id).await;
    assert_eq!(
        hers.iter().map(label).collect::<Vec<_>>(),
        vec!["conversation_created"]
    );
    assert!(ada_ws.events_until_marker(marker.user.id).await.is_empty());
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn channel_details_are_validated(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let path = paths::channels(app.workspace_id);
    for (topic, description) in [
        (json!("line\nbreak"), json!(null)),
        (json!("x".repeat(251)), json!(null)),
        (json!(null), json!("y".repeat(1001))),
        (json!(null), json!("bell\u{7}")),
    ] {
        let body = json!({"operation_id": OperationId::generate(), "name": "ok-name", "visibility": "public", "topic": topic, "description": description});
        denied(
            app.call(Method::POST, &path, &ada, Some(body)).await,
            ErrorCode::InvalidRequest,
        )
        .await;
    }
    // Nothing was created by the rejected attempts.
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM conversations WHERE name = 'ok-name'")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    // Empty text means none.
    let body = json!({"operation_id": OperationId::generate(), "name": "ok-name", "visibility": "public", "topic": "", "description": ""});
    let channel = conversation_of(app.call(Method::POST, &path, &ada, Some(body)).await).await;
    assert_eq!((channel.topic, channel.description), (None, None));
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn people_outside_the_workspace_cannot_create_channels(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let stranger = app.register("stranger").await;
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
        .bind(stranger.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    denied(
        app.create_channel_raw(&stranger, "nope", "public", OperationId::generate())
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.open_raw(&stranger, &[&ada], OperationId::generate())
            .await,
        ErrorCode::NotFound,
    )
    .await;
    let browse = app
        .call(
            Method::GET,
            &paths::browse_channels(app.workspace_id),
            &stranger,
            None,
        )
        .await;
    denied(browse, ErrorCode::NotFound).await;
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn simultaneous_creates_of_one_name_have_exactly_one_winner(pool: PgPool) {
    let app = App::start(pool).await;
    let mut people = Vec::new();
    for index in 0..8 {
        people.push(app.register(&format!("person{index}")).await);
    }
    let baseline = app.head().await;

    let attempts: Vec<_> = people
        .iter()
        .map(|person| {
            let http = app.http.clone();
            let url = app.url(&paths::channels(app.workspace_id));
            let token = person.token.clone();
            tokio::spawn(async move {
                let body = json!({"operation_id": OperationId::generate(), "name": "contested", "visibility": "public", "topic": null, "description": null});
                let response = http.post(url).bearer_auth(token).json(&body).send().await.unwrap();
                (response.status().as_u16(), response.text().await.unwrap())
            })
        })
        .collect();
    let mut winners = 0;
    for attempt in attempts {
        let (status, body) = attempt.await.unwrap();
        match status {
            200 => winners += 1,
            409 => assert!(body.contains("name_taken"), "{body}"),
            other => panic!("unexpected status {other}: {body}"),
        }
    }
    assert_eq!(winners, 1);
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM conversations WHERE name = 'contested'")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(event_count(&app, "conversation_created", baseline).await, 1);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn simultaneous_repeats_of_one_create_return_the_same_channel(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let baseline = app.head().await;
    let operation = OperationId::generate();

    let attempts: Vec<_> = (0..12)
        .map(|_| {
            let http = app.http.clone();
            let url = app.url(&paths::channels(app.workspace_id));
            let token = ada.token.clone();
            tokio::spawn(async move {
                let body = json!({"operation_id": operation, "name": "once", "visibility": "public", "topic": null, "description": null});
                http.post(url).bearer_auth(token).json(&body).send().await.unwrap()
            })
        })
        .collect();
    let mut ids = std::collections::HashSet::new();
    for attempt in attempts {
        let response = attempt.await.unwrap();
        assert!(response.status().is_success(), "{}", response.status());
        ids.insert(
            response
                .json::<ConversationResponse>()
                .await
                .unwrap()
                .conversation
                .id,
        );
    }
    assert_eq!(ids.len(), 1);
    assert_eq!(event_count(&app, "conversation_created", baseline).await, 1);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn opening_a_conversation_is_idempotent_per_participant_set(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;
    let hopper = app.register("hopper").await;
    let mut ada_ws = subscribed(&app, &ada).await;
    let mut hopper_ws = subscribed(&app, &hopper).await;

    let direct = app.open(&ada, &[&grace]).await;
    assert_eq!(direct.kind, ConversationKind::Direct);
    assert_eq!(direct.name, None);
    assert_eq!(direct.visibility, ChannelVisibility::Private);
    assert_eq!(direct.member_count, 2);
    let mut expected = vec![ada.user.id, grace.user.id];
    expected.sort();
    assert_eq!(direct.participants, expected);
    assert_eq!(direct.created_by, Some(ada.user.id));

    // Same people in any order, from either side, with or without naming yourself, with any
    // operation id: the same conversation, and no further event.
    let head = app.head().await;
    assert_eq!(app.open(&grace, &[&ada]).await.id, direct.id);
    assert_eq!(app.open(&ada, &[&grace, &ada, &grace]).await.id, direct.id);
    assert_eq!(app.head().await, head);

    // A third person makes a different, empty conversation; the first is untouched.
    let group = app.open(&ada, &[&grace, &hopper]).await;
    assert_ne!(group.id, direct.id);
    assert_eq!(group.kind, ConversationKind::Group);
    assert_eq!(group.participants.len(), 3);
    assert_eq!(group.member_count, 3);

    // Both appear in the right sidebars with participants listed.
    assert!(
        app.conversations_of(&grace)
            .await
            .iter()
            .any(|c| c.id == direct.id)
    );
    assert!(
        !app.conversations_of(&hopper)
            .await
            .iter()
            .any(|c| c.id == direct.id)
    );
    assert!(
        app.conversations_of(&hopper)
            .await
            .iter()
            .any(|c| c.id == group.id && c.participants.len() == 3)
    );

    // Only the people in each conversation heard about it.
    let marker = app.register("marker").await;
    let ada_events = ada_ws.events_until_marker(marker.user.id).await;
    assert_eq!(
        ada_events.iter().map(label).collect::<Vec<_>>(),
        vec!["conversation_created", "conversation_created"]
    );
    let hopper_events = hopper_ws.events_until_marker(marker.user.id).await;
    assert_eq!(hopper_events.len(), 1, "hopper only hears of the group");
    assert!(
        matches!(&hopper_events[0].event, Event::ConversationCreated { conversation } if conversation.id == group.id)
    );
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn opening_a_conversation_validates_the_people(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;
    let outsider = app.register("outsider").await;
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
        .bind(outsider.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();

    // Nobody else, only yourself, someone outside the workspace, someone who does not exist.
    denied(
        app.open_raw(&ada, &[], OperationId::generate()).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    denied(
        app.open_raw(&ada, &[&ada], OperationId::generate()).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    denied(
        app.open_raw(&ada, &[&outsider], OperationId::generate())
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    let unknown = json!({"operation_id": OperationId::generate(), "participants": [lazy_domain::UserId::generate()]});
    let path = paths::open_conversation(app.workspace_id);
    denied(
        app.call(Method::POST, &path, &ada, Some(unknown)).await,
        ErrorCode::InvalidRequest,
    )
    .await;

    // At most nine people altogether.
    let mut others = Vec::new();
    for index in 0..9 {
        others.push(app.register(&format!("crowd{index}")).await);
    }
    let refs: Vec<&Account> = others.iter().collect();
    denied(
        app.open_raw(&ada, &refs, OperationId::generate()).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    assert_eq!(app.open(&ada, &refs[..8]).await.member_count, 9);

    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM conversations WHERE kind <> 'channel'")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(rows, 1, "rejected requests created nothing");
    let _ = grace;
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn open_operations_are_reconcilable_and_conflict_when_reused(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;
    let hopper = app.register("hopper").await;
    let operation = OperationId::generate();

    let first = conversation_of(app.open_raw(&ada, &[&grace], operation).await).await;
    match app.operation(&ada, operation).await {
        OperationStatus::ConversationCommitted { conversation } => assert_eq!(conversation, first),
        other => panic!("unexpected status {other:?}"),
    }
    // The same operation again: the same conversation. With other people: a conflict.
    assert_eq!(
        conversation_of(app.open_raw(&ada, &[&grace], operation).await)
            .await
            .id,
        first.id
    );
    denied(
        app.open_raw(&ada, &[&hopper], operation).await,
        ErrorCode::Conflict,
    )
    .await;
    // Create and open are different commands even with the same id.
    denied(
        app.create_channel_raw(&ada, "mixed", "public", operation)
            .await,
        ErrorCode::Conflict,
    )
    .await;

    // A different operation id that finds the existing conversation can also be looked up.
    let second = OperationId::generate();
    assert_eq!(
        conversation_of(app.open_raw(&grace, &[&ada], second).await)
            .await
            .id,
        first.id
    );
    assert!(matches!(
        app.operation(&grace, second).await,
        OperationStatus::ConversationCommitted { conversation } if conversation.id == first.id
    ));
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn simultaneous_opens_of_one_set_create_exactly_one_conversation(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;
    let baseline = app.head().await;

    // Both people open the same conversation at the same time, over and over.
    let attempts: Vec<_> = (0..16)
        .map(|index| {
            let (opener, other) = if index % 2 == 0 {
                (&ada, &grace)
            } else {
                (&grace, &ada)
            };
            let http = app.http.clone();
            let url = app.url(&paths::open_conversation(app.workspace_id));
            let token = opener.token.clone();
            let participants = vec![other.user.id];
            tokio::spawn(async move {
                let body =
                    json!({"operation_id": OperationId::generate(), "participants": participants});
                http.post(url)
                    .bearer_auth(token)
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
            })
        })
        .collect();
    let mut ids = std::collections::HashSet::new();
    for attempt in attempts {
        let response = attempt.await.unwrap();
        assert!(response.status().is_success(), "{}", response.status());
        ids.insert(
            response
                .json::<ConversationResponse>()
                .await
                .unwrap()
                .conversation
                .id,
        );
    }
    assert_eq!(ids.len(), 1, "everyone got the same conversation");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM conversations WHERE kind = 'direct'")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(event_count(&app, "conversation_created", baseline).await, 1);
    let id = *ids.iter().next().unwrap();
    assert_eq!(app.counted_members(id).await, 2);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn direct_conversations_are_private_to_their_participants(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;
    let hopper = app.register("hopper").await;
    let direct = app.open(&ada, &[&grace]).await;
    let mut grace_ws = subscribed(&app, &grace).await;
    let mut hopper_ws = subscribed(&app, &hopper).await;

    let message = app.send(&ada, direct.id, "just us").await;
    assert_eq!(message.conversation_id, direct.id);
    // Hopper can neither read nor write it, and does not know it exists.
    denied(
        app.history(&hopper, direct.id, &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.send_raw(&hopper, direct.id, OperationId::generate(), "hi")
            .await,
        ErrorCode::NotFound,
    )
    .await;
    assert_eq!(
        app.history_ok(&grace, direct.id, &[]).await.messages,
        vec![message]
    );

    let marker = app.register("marker").await;
    let hers = grace_ws.events_until_marker(marker.user.id).await;
    assert_eq!(
        hers.iter().map(label).collect::<Vec<_>>(),
        vec!["message_created"]
    );
    assert!(
        hopper_ws
            .events_until_marker(marker.user.id)
            .await
            .is_empty()
    );

    // Nobody can leave, archive, or add to a direct conversation.
    denied(
        app.act(&grace, direct.id, "leave").await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.act(&grace, direct.id, "archive").await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.add_members(&grace, direct.id, &[&hopper]).await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.update(&grace, direct.id, json!({"topic": "hello"}))
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.remove_member(&ada, direct.id, &grace).await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.act(&hopper, direct.id, "leave").await,
        ErrorCode::NotFound,
    )
    .await;
    app.server.shutdown().await.unwrap();
}
