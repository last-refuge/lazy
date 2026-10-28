//! Read positions, unread counts, and favorites.

mod common;

use common::*;
use lazy_protocol::{ConversationUserState, ErrorCode, Event, UserStateResponse};
use serde_json::json;
use sqlx::PgPool;

async fn denied(response: reqwest::Response, code: ErrorCode) {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
}

async fn state_response(response: reqwest::Response) -> ConversationUserState {
    assert!(
        response.status().is_success(),
        "status {}",
        response.status()
    );
    response.json::<UserStateResponse>().await.unwrap().state
}

async fn subscribed(app: &App, who: &Account) -> Ws {
    let mut ws = app.connect_ws(who).await;
    ws.subscribe(app.workspace_id, app.head().await).await;
    ws.replay().await;
    ws
}

fn state_events(events: &[lazy_protocol::EventEnvelope]) -> Vec<ConversationUserState> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::UserStateChanged { state } => Some(state.clone()),
            _ => None,
        })
        .collect()
}

#[sqlx::test]
async fn unread_counts_other_peoples_messages_after_the_read_position(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;

    // Ada has not looked since she joined; grace writes three messages.
    let mut sent = Vec::new();
    for text in ["one", "two", "three"] {
        sent.push(app.send(&grace, app.general, text).await);
    }
    let ada_state = app.state_of(&ada, app.general).await;
    assert_eq!((ada_state.last_read.get(), ada_state.unread_count), (0, 3));
    assert_eq!(ada_state.mention_count, 0);
    // Grace wrote them, so for her they are read: her position moved to her newest message.
    let grace_state = app.state_of(&grace, app.general).await;
    assert_eq!(
        (grace_state.last_read, grace_state.unread_count),
        (sent[2].seq, 0)
    );

    // Ada replies. That moves her own position to her message (reading past grace's) without an
    // extra event, and gives grace one unread.
    let mut ada_ws = subscribed(&app, &ada).await;
    let reply = app.send(&ada, app.general, "hello back").await;
    let ada_state = app.state_of(&ada, app.general).await;
    assert_eq!(
        (ada_state.last_read, ada_state.unread_count),
        (reply.seq, 0)
    );
    let grace_state = app.state_of(&grace, app.general).await;
    assert_eq!(
        (grace_state.last_read, grace_state.unread_count),
        (sent[2].seq, 1)
    );

    let marker = app.register("marker").await;
    let events = ada_ws.events_until_marker(marker.user.id).await;
    assert!(
        events
            .iter()
            .all(|e| matches!(e.event, Event::MessageCreated { .. })),
        "sending moved the read position without announcing it"
    );
    assert_eq!(events.len(), 1);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn marking_read_only_moves_forward_and_is_clamped_to_the_newest_message(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;
    let mut sent = Vec::new();
    for n in 0..5 {
        sent.push(app.send(&grace, app.general, &format!("m{n}")).await);
    }
    // Two devices of ada, and grace.
    let mut phone = subscribed(&app, &ada).await;
    let mut laptop = subscribed(&app, &ada).await;
    let mut grace_ws = subscribed(&app, &grace).await;

    let state = state_response(app.mark_read(&ada, app.general, sent[1].seq.get()).await).await;
    assert_eq!((state.last_read, state.unread_count), (sent[1].seq, 3));

    // Moving backwards, or to where it already is, changes nothing and announces nothing.
    let head = app.head().await;
    let same = state_response(app.mark_read(&ada, app.general, sent[0].seq.get()).await).await;
    assert_eq!(same, state);
    let same = state_response(app.mark_read(&ada, app.general, sent[1].seq.get()).await).await;
    assert_eq!(same, state);
    assert_eq!(app.head().await, head);

    // Beyond the newest message: clamped to it, never to a position that does not exist yet.
    let state = state_response(app.mark_read(&ada, app.general, 1_000_000).await).await;
    assert_eq!((state.last_read, state.unread_count), (sent[4].seq, 0));
    let later = app.send(&grace, app.general, "after").await;
    assert_eq!(app.state_of(&ada, app.general).await.unread_count, 1);
    assert_eq!(app.state_of(&ada, app.general).await.last_read, sent[4].seq);
    assert!(later.seq > sent[4].seq);

    // Both of ada's devices hear each advance, in order, and grace hears nothing of them.
    let marker = app.register("marker").await;
    for device in [&mut phone, &mut laptop] {
        let seen = state_events(&device.events_until_marker(marker.user.id).await);
        let positions: Vec<_> = seen.iter().map(|s| s.last_read).collect();
        assert_eq!(positions, vec![sent[1].seq, sent[4].seq]);
        assert_eq!(seen[0].unread_count, 3);
    }
    assert!(state_events(&grace_ws.events_until_marker(marker.user.id).await).is_empty());
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn an_empty_conversation_has_nothing_to_mark_read(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let quiet = app.create_channel(&ada, "quiet", "public").await;
    let head = app.head().await;
    let state = state_response(app.mark_read(&ada, quiet.id, 50).await).await;
    assert_eq!((state.last_read.get(), state.unread_count), (0, 0));
    assert_eq!(
        app.head().await,
        head,
        "clamped to nothing, so nothing changed"
    );
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn only_members_have_read_state(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;
    let public = app.create_channel(&ada, "public-room", "public").await;
    let private = app.create_channel(&ada, "private-room", "private").await;

    // A public channel is visible but not theirs to mark; a private one does not exist for them.
    denied(
        app.mark_read(&grace, public.id, 1).await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.favorite(&grace, public.id, true).await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.mark_read(&grace, private.id, 1).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.favorite(&grace, private.id, true).await,
        ErrorCode::NotFound,
    )
    .await;
    let body = json!({"up_to": -1});
    denied(
        app.call(
            reqwest::Method::POST,
            &lazy_protocol::api::paths::conversation_action(app.general, "read"),
            &grace,
            Some(body),
        )
        .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn favorites_are_personal_and_sync_to_the_owners_devices_only(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;
    let mut phone = subscribed(&app, &ada).await;
    let mut laptop = subscribed(&app, &ada).await;
    let mut grace_ws = subscribed(&app, &grace).await;

    let starred = state_response(app.favorite(&ada, app.general, true).await).await;
    assert!(starred.favorite);
    // Setting it again changes nothing and announces nothing.
    let head = app.head().await;
    assert!(
        state_response(app.favorite(&ada, app.general, true).await)
            .await
            .favorite
    );
    assert_eq!(app.head().await, head);

    assert!(app.state_of(&ada, app.general).await.favorite);
    assert!(
        !app.state_of(&grace, app.general).await.favorite,
        "favorites are per person"
    );

    let unstarred = state_response(app.favorite(&ada, app.general, false).await).await;
    assert!(!unstarred.favorite);

    let marker = app.register("marker").await;
    for device in [&mut phone, &mut laptop] {
        let seen = state_events(&device.events_until_marker(marker.user.id).await);
        assert_eq!(
            seen.iter().map(|s| s.favorite).collect::<Vec<_>>(),
            vec![true, false]
        );
    }
    assert!(state_events(&grace_ws.events_until_marker(marker.user.id).await).is_empty());
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn joining_late_starts_with_nothing_unread_and_leaving_forgets_the_state(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;
    let team = app.create_channel(&ada, "team", "public").await;
    let mut history = Vec::new();
    for n in 0..4 {
        history.push(app.send(&ada, team.id, &format!("before {n}")).await);
    }

    // A late joiner does not face a wall of unread; history is theirs to scroll back through.
    app.act(&grace, team.id, "join").await;
    let state = app.state_of(&grace, team.id).await;
    assert_eq!((state.last_read, state.unread_count), (history[3].seq, 0));
    app.send(&ada, team.id, "after").await;
    assert_eq!(app.state_of(&grace, team.id).await.unread_count, 1);

    // Favorites go with the membership: leave and rejoin and it starts fresh.
    app.favorite(&grace, team.id, true).await;
    app.act(&grace, team.id, "leave").await;
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM conversation_user_state WHERE user_id = $1 AND conversation_id = $2",
    )
    .bind(grace.user.id.as_uuid())
    .bind(team.id.as_uuid())
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(rows, 0);
    app.act(&grace, team.id, "join").await;
    let state = app.state_of(&grace, team.id).await;
    assert!(!state.favorite);
    assert_eq!(state.unread_count, 0);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn direct_conversations_track_unread_per_participant(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;
    let direct = app.open(&ada, &[&grace]).await;

    app.send(&ada, direct.id, "psst").await;
    app.send(&ada, direct.id, "are you there?").await;
    assert_eq!(app.state_of(&grace, direct.id).await.unread_count, 2);
    assert_eq!(app.state_of(&ada, direct.id).await.unread_count, 0);
    let last = app
        .history_ok(&grace, direct.id, &[])
        .await
        .messages
        .last()
        .unwrap()
        .seq;
    state_response(app.mark_read(&grace, direct.id, last.get()).await).await;
    assert_eq!(app.state_of(&grace, direct.id).await.unread_count, 0);
    app.server.shutdown().await.unwrap();
}
