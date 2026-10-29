//! Editing and deleting messages: results, events, authorization, idempotency, what disappears on
//! deletion, replay, and the races between edits and deletes.

mod common;

use common::*;
use futures_util::future::join_all;
use lazy_domain::message::deleted_placeholder;
use lazy_domain::{EventSeq, MessageDocument, MessageId, OperationId};
use lazy_protocol::{
    ErrorCode, Event, EventEnvelope, Message, MessageResponse, OperationStatus, PROTOCOL_VERSION,
};
use serde_json::json;
use sqlx::PgPool;

async fn denied(response: reqwest::Response, code: ErrorCode) {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
}

async fn message_of(response: reqwest::Response) -> Message {
    assert!(
        response.status().is_success(),
        "status {}",
        response.status()
    );
    response.json::<MessageResponse>().await.unwrap().message
}

/// One page of a conversation's history, optionally before a position.
async fn history_page(
    app: &App,
    who: &Account,
    before: Option<EventSeq>,
    limit: u32,
) -> lazy_protocol::HistoryPage {
    let mut query = vec![("limit", limit.to_string())];
    if let Some(before) = before {
        query.push(("before", before.get().to_string()));
    }
    app.history_ok(who, app.general, &query).await
}

async fn subscribed(app: &App, who: &Account) -> Ws {
    let mut ws = app.connect_ws(who).await;
    ws.subscribe(app.workspace_id, app.head().await).await;
    ws.replay().await;
    ws
}

/// Events concerning messages, in order.
fn message_events(events: &[EventEnvelope]) -> Vec<&Event> {
    events
        .iter()
        .map(|e| &e.event)
        .filter(|e| {
            matches!(
                e,
                Event::MessageCreated { .. }
                    | Event::MessageEdited { .. }
                    | Event::MessageDeleted { .. }
            )
        })
        .collect()
}

/// An owner, an admin, an author and a member, all in #general; plus a stranger who is in the
/// workspace but is not in the private channels the tests create.
struct Chat {
    app: App,
    owner: Account,
    admin: Account,
    author: Account,
    member: Account,
    stranger: Account,
}

async fn chat(pool: PgPool) -> Chat {
    let app = App::start(pool).await;
    let owner = app.register("owner").await;
    let admin = app.register("admin").await;
    app.set_role(&admin, "admin").await;
    let author = app.register("author").await;
    let member = app.register("member").await;
    let stranger = app.register("stranger").await;
    Chat {
        app,
        owner,
        admin,
        author,
        member,
        stranger,
    }
}

async fn finish(chat: Chat) {
    chat.app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn an_edit_replaces_the_content_and_keeps_the_messages_place(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let original = app.send(&c.author, app.general, "first draft").await;
    let mut author_ws = subscribed(app, &c.author).await;
    let mut member_ws = subscribed(app, &c.member).await;

    let operation = OperationId::generate();
    let edited = message_of(
        app.edit_raw(
            &c.author,
            original.id,
            operation,
            1,
            "*final* [docs](https://example.com)",
        )
        .await,
    )
    .await;
    assert_eq!(
        (
            edited.id,
            edited.conversation_id,
            edited.author_id,
            edited.seq,
            edited.created_at
        ),
        (
            original.id,
            original.conversation_id,
            original.author_id,
            original.seq,
            original.created_at
        ),
        "an edit never moves a message"
    );
    assert_eq!((edited.revision, edited.deleted), (2, false));
    assert!(edited.edited_at.is_some());
    assert_eq!(
        edited.document,
        MessageDocument::from_markdown("*final* [docs](https://example.com)").unwrap()
    );
    // The text projection (for search and notifications) follows the edit, as the domain defines it.
    let projection: String = sqlx::query_scalar("SELECT plain_text FROM messages WHERE id = $1")
        .bind(original.id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(projection, edited.document.plain_text());
    assert!(projection.starts_with("final docs"));

    // Everyone sees the edited message in history and when fetching it.
    assert_eq!(
        app.history_ok(&c.member, app.general, &[]).await.messages,
        vec![edited.clone()]
    );
    assert_eq!(app.get_message(&c.member, original.id).await, edited);
    // The previous revision is kept on the server, with the original's (absent) edit time.
    assert_eq!(app.kept_revisions(original.id).await, vec![1]);
    let kept: (String, Option<time::OffsetDateTime>) = sqlx::query_as(
        "SELECT document::text, edited_at FROM message_revisions WHERE message_id = $1 AND revision = 1",
    )
    .bind(original.id.as_uuid())
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert!(kept.0.contains("first draft"));
    assert_eq!(kept.1, None);

    // The members are told, once, with the editor's operation id.
    let marker = app.register("marker").await;
    for ws in [&mut author_ws, &mut member_ws] {
        let events = ws.events_until_marker(marker.user.id).await;
        assert_eq!(events.len(), 1);
        match &events[0].event {
            Event::MessageEdited {
                message,
                operation_id,
                ..
            } => {
                assert_eq!(message, &edited);
                assert_eq!(*operation_id, Some(operation));
            }
            other => panic!("unexpected event {other:?}"),
        }
        assert!(
            events[0].seq > original.seq,
            "the event is new; the message is not"
        );
    }
    match app.operation(&c.author, operation).await {
        OperationStatus::EditCommitted { message } => assert_eq!(message, edited),
        other => panic!("unexpected status {other:?}"),
    }
    finish(c).await;
}

#[sqlx::test]
async fn an_edit_from_an_old_revision_is_refused_and_changes_nothing(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "one").await;
    let second = app.edit(&c.author, &message, "two").await;
    let head = app.head().await;

    // The editor started from revision 1, which has moved on. Any other revision is stale too.
    for base in [1, 3, 99] {
        let response = app
            .edit_raw(
                &c.author,
                message.id,
                OperationId::generate(),
                base,
                "three",
            )
            .await;
        denied(response, ErrorCode::StaleRevision).await;
    }
    assert_eq!(app.get_message(&c.author, message.id).await, second);
    assert_eq!(app.head().await, head, "a refused edit appends nothing");
    assert_eq!(app.kept_revisions(message.id).await, vec![1]);

    // From the current revision it goes through.
    let third = app.edit(&c.author, &second, "three").await;
    assert_eq!(third.revision, 3);
    assert_eq!(app.kept_revisions(message.id).await, vec![1, 2]);
    finish(c).await;
}

#[sqlx::test]
async fn repeating_an_edit_returns_its_original_result_even_after_later_edits(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "v1").await;

    let (op_a, op_b) = (OperationId::generate(), OperationId::generate());
    let first = message_of(app.edit_raw(&c.author, message.id, op_a, 1, "v2").await).await;
    let second = message_of(app.edit_raw(&c.author, message.id, op_b, 2, "v3").await).await;
    assert_eq!((first.revision, second.revision), (2, 3));

    // The first request again, long after the message moved to revision 3: the same result, and
    // nothing new in the log.
    let head = app.head().await;
    let again = message_of(app.edit_raw(&c.author, message.id, op_a, 1, "v2").await).await;
    assert_eq!(again, first);
    assert_eq!(again.document.plain_text(), "v2");
    assert_eq!(app.head().await, head);
    match app.operation(&c.author, op_a).await {
        OperationStatus::EditCommitted { message } => assert_eq!(message, first),
        other => panic!("unexpected status {other:?}"),
    }
    match app.operation(&c.author, op_b).await {
        OperationStatus::EditCommitted { message } => assert_eq!(message, second),
        other => panic!("unexpected status {other:?}"),
    }
    // Operation ids belong to the person who made them.
    assert_eq!(
        app.operation(&c.member, op_a).await,
        OperationStatus::NotFound
    );
    finish(c).await;
}

#[sqlx::test]
async fn an_operation_id_cannot_be_reused_for_something_else(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "v1").await;
    let other = app.send(&c.author, app.general, "other").await;
    let operation = OperationId::generate();
    message_of(
        app.edit_raw(&c.author, message.id, operation, 1, "v2")
            .await,
    )
    .await;

    // Different words, different base revision, a different message: all conflicts.
    denied(
        app.edit_raw(&c.author, message.id, operation, 1, "different")
            .await,
        ErrorCode::Conflict,
    )
    .await;
    denied(
        app.edit_raw(&c.author, message.id, operation, 2, "v2")
            .await,
        ErrorCode::Conflict,
    )
    .await;
    denied(
        app.edit_raw(&c.author, other.id, operation, 1, "v2").await,
        ErrorCode::Conflict,
    )
    .await;
    // So is using the id of a send, or of a channel creation.
    let send_operation = OperationId::generate();
    app.send_raw(&c.author, app.general, send_operation, "sent")
        .await;
    denied(
        app.edit_raw(&c.author, message.id, send_operation, 2, "x")
            .await,
        ErrorCode::Conflict,
    )
    .await;
    denied(
        app.create_channel_raw(&c.author, "clash", "public", operation)
            .await,
        ErrorCode::Conflict,
    )
    .await;

    assert_eq!(
        app.get_message(&c.author, message.id)
            .await
            .document
            .plain_text(),
        "v2"
    );
    assert_eq!(app.get_message(&c.author, other.id).await.revision, 1);
    finish(c).await;
}

#[sqlx::test]
async fn an_edit_that_changes_nothing_commits_nothing(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "same words").await;
    let mut ws = subscribed(app, &c.member).await;
    let head = app.head().await;

    let operation = OperationId::generate();
    let result = message_of(
        app.edit_raw(&c.author, message.id, operation, 1, "same words")
            .await,
    )
    .await;
    assert_eq!(result, message, "the current message, unchanged");
    assert_eq!(app.head().await, head, "no event, no position");
    assert!(app.kept_revisions(message.id).await.is_empty());

    // The receipt is recorded anyway, so a lost response can be reconciled.
    match app.operation(&c.author, operation).await {
        OperationStatus::EditCommitted { message: found } => assert_eq!(found, message),
        other => panic!("unexpected status {other:?}"),
    }
    assert_eq!(
        message_of(
            app.edit_raw(&c.author, message.id, operation, 1, "same words")
                .await
        )
        .await,
        message
    );
    // After a real edit, repeating the no-op returns what it originally found: revision 1.
    app.edit(&c.author, &message, "changed").await;
    let again = message_of(
        app.edit_raw(&c.author, message.id, operation, 1, "same words")
            .await,
    )
    .await;
    assert_eq!(
        (again.revision, again.document.plain_text()),
        (1, "same words".into())
    );

    let marker = app.register("marker").await;
    let events = ws.events_until_marker(marker.user.id).await;
    assert_eq!(
        message_events(&events).len(),
        1,
        "only the real edit was announced"
    );
    finish(c).await;
}

#[sqlx::test]
async fn only_the_author_edits_and_nothing_hidden_is_revealed(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "mine").await;

    // Not the author: forbidden for a member, an admin, and the owner alike.
    for other in [&c.member, &c.admin, &c.owner] {
        denied(
            app.edit_raw(other, message.id, OperationId::generate(), 1, "hijack")
                .await,
            ErrorCode::Forbidden,
        )
        .await;
    }
    // A private channel does not exist for people outside it.
    let hush = app.create_channel(&c.author, "hush", "private").await;
    let secret = app.send(&c.author, hush.id, "secret").await;
    for outsider in [&c.stranger, &c.admin, &c.owner] {
        denied(
            app.edit_raw(outsider, secret.id, OperationId::generate(), 1, "peek")
                .await,
            ErrorCode::NotFound,
        )
        .await;
        denied(
            app.get_message_raw(outsider, secret.id).await,
            ErrorCode::NotFound,
        )
        .await;
        denied(
            app.delete_raw(outsider, secret.id).await,
            ErrorCode::NotFound,
        )
        .await;
    }
    // A message that does not exist, and a person who left the workspace.
    denied(
        app.edit_raw(
            &c.author,
            MessageId::generate(),
            OperationId::generate(),
            1,
            "x",
        )
        .await,
        ErrorCode::NotFound,
    )
    .await;
    let gone = app.register("gone").await;
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
        .bind(gone.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    denied(
        app.get_message_raw(&gone, message.id).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.edit_raw(&gone, message.id, OperationId::generate(), 1, "x")
            .await,
        ErrorCode::NotFound,
    )
    .await;

    // None of that changed anything or left a receipt.
    assert_eq!(app.get_message(&c.author, message.id).await, message);
    assert_eq!(app.get_message(&c.author, secret.id).await, secret);
    assert!(app.kept_revisions(message.id).await.is_empty());
    let receipts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM operation_receipts WHERE kind = 'edit_message'")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(receipts, 0);
    finish(c).await;
}

#[sqlx::test]
async fn an_edit_of_a_deleted_message_is_not_found(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "soon gone").await;
    app.delete(&c.author, message.id).await;
    denied(
        app.edit_raw(&c.author, message.id, OperationId::generate(), 1, "revive")
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.edit_raw(&c.author, message.id, OperationId::generate(), 2, "revive")
            .await,
        ErrorCode::NotFound,
    )
    .await;
    assert!(app.get_message(&c.author, message.id).await.deleted);
    finish(c).await;
}

#[sqlx::test]
async fn archiving_closes_a_channel_to_its_authors_but_not_to_moderators(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let team = app.create_channel(&c.author, "team", "public").await;
    assert!(
        app.act(&c.member, team.id, "join")
            .await
            .status()
            .is_success()
    );
    let theirs = app.send(&c.member, team.id, "from the member").await;
    let mine = app.send(&c.author, team.id, "from the author").await;
    assert!(
        app.act(&c.author, team.id, "archive")
            .await
            .status()
            .is_success()
    );

    // The people who wrote the messages can no longer change them.
    denied(
        app.edit_raw(&c.member, theirs.id, OperationId::generate(), 1, "x")
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.delete_raw(&c.member, theirs.id).await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.edit_raw(&c.author, mine.id, OperationId::generate(), 1, "x")
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.delete_raw(&c.author, mine.id).await,
        ErrorCode::Forbidden,
    )
    .await;
    // Moderators still can delete.
    assert!(app.delete(&c.admin, theirs.id).await.deleted);
    assert!(app.delete(&c.owner, mine.id).await.deleted);

    // Reopened, authors edit again.
    assert!(
        app.act(&c.author, team.id, "unarchive")
            .await
            .status()
            .is_success()
    );
    let later = app.send(&c.member, team.id, "after reopening").await;
    assert_eq!(
        app.edit(&c.member, &later, "edited after reopening")
            .await
            .revision,
        2
    );
    finish(c).await;
}

#[sqlx::test]
async fn who_may_delete_a_message(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;

    // Authors delete their own; other members cannot; admins and the owner can.
    let own = app.send(&c.author, app.general, "my words").await;
    denied(
        app.delete_raw(&c.member, own.id).await,
        ErrorCode::Forbidden,
    )
    .await;
    let tombstone = app.delete(&c.author, own.id).await;
    assert!(tombstone.deleted);
    assert_eq!(tombstone.document, deleted_placeholder());
    assert_eq!(
        (
            tombstone.id,
            tombstone.author_id,
            tombstone.conversation_id,
            tombstone.seq,
            tombstone.created_at
        ),
        (
            own.id,
            own.author_id,
            own.conversation_id,
            own.seq,
            own.created_at
        ),
        "a tombstone keeps its identity and place"
    );
    assert_eq!((tombstone.revision, tombstone.edited_at), (2, None));
    for moderator in [&c.admin, &c.owner] {
        let theirs = app.send(&c.author, app.general, "to be moderated").await;
        assert!(app.delete(moderator, theirs.id).await.deleted);
    }

    // A private channel keeps even moderators out, so deleting there is "not found" and the
    // message survives.
    let hush = app.create_channel(&c.author, "hush", "private").await;
    let secret = app.send(&c.author, hush.id, "secret").await;
    for outsider in [&c.admin, &c.owner, &c.stranger] {
        denied(
            app.delete_raw(outsider, secret.id).await,
            ErrorCode::NotFound,
        )
        .await;
    }
    assert!(!app.get_message(&c.author, secret.id).await.deleted);

    // Someone who has left can no longer change their own words, but a public channel is still
    // visible to them, and a private one is not.
    let team = app.create_channel(&c.author, "team", "public").await;
    assert!(
        app.act(&c.member, team.id, "join")
            .await
            .status()
            .is_success()
    );
    let said = app.send(&c.member, team.id, "I said this").await;
    assert!(
        app.act(&c.member, team.id, "leave")
            .await
            .status()
            .is_success()
    );
    denied(
        app.delete_raw(&c.member, said.id).await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.edit_raw(&c.member, said.id, OperationId::generate(), 1, "x")
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    assert!(
        app.add_members(&c.author, hush.id, &[&c.member])
            .await
            .status()
            .is_success()
    );
    let hushed = app.send(&c.member, hush.id, "private words").await;
    assert!(
        app.remove_member(&c.author, hush.id, &c.member)
            .await
            .status()
            .is_success()
    );
    denied(
        app.delete_raw(&c.member, hushed.id).await,
        ErrorCode::NotFound,
    )
    .await;
    finish(c).await;
}

#[sqlx::test]
async fn deleting_twice_returns_the_tombstone_and_appends_nothing_the_second_time(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "delete me").await;
    let first = app.delete(&c.author, message.id).await;
    let head = app.head().await;
    assert_eq!(app.delete(&c.author, message.id).await, first);
    assert_eq!(
        app.delete(&c.admin, message.id).await,
        first,
        "an admin could have deleted it too"
    );
    assert_eq!(app.head().await, head);
    let kinds: Vec<String> = app
        .message_events(message.id)
        .await
        .into_iter()
        .map(|(_, kind)| kind)
        .collect();
    assert_eq!(kinds, vec!["message_created", "message_deleted"]);

    // Someone who could not have deleted it is still refused.
    denied(
        app.delete_raw(&c.member, message.id).await,
        ErrorCode::Forbidden,
    )
    .await;
    // Members can fetch the tombstone; people outside a private channel cannot.
    assert_eq!(app.get_message(&c.member, message.id).await, first);
    let hush = app.create_channel(&c.author, "hush", "private").await;
    let secret = app.send(&c.author, hush.id, "x").await;
    app.delete(&c.author, secret.id).await;
    denied(
        app.get_message_raw(&c.stranger, secret.id).await,
        ErrorCode::NotFound,
    )
    .await;
    finish(c).await;
}

#[sqlx::test]
async fn deleted_messages_leave_history_and_unread_counts(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    // g0 (deleted), g1, g2 (deleted), g3, g4, g5 (deleted), g6, all by the author.
    let mut sent = Vec::new();
    for n in 0..7 {
        sent.push(app.send(&c.author, app.general, &format!("g{n}")).await);
    }
    for n in [0, 2, 5] {
        app.delete(&c.author, sent[n].id).await;
    }
    let visible = [&sent[1], &sent[3], &sent[4], &sent[6]];

    let all = app.history_ok(&c.member, app.general, &[]).await;
    assert_eq!(
        all.messages.iter().map(|m| m.id).collect::<Vec<_>>(),
        visible.iter().map(|m| m.id).collect::<Vec<_>>()
    );
    assert!(!all.has_more);

    // The page holds `limit` messages that exist, and `has_more` counts only ones that exist,
    // including when deleted messages sit at the edge of a page.
    let newest = history_page(app, &c.member, None, 2).await;
    assert_eq!(
        newest.messages.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![sent[4].id, sent[6].id]
    );
    assert!(newest.has_more);
    let older = history_page(app, &c.member, Some(sent[4].seq), 2).await;
    assert_eq!(
        older.messages.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![sent[1].id, sent[3].id]
    );
    assert!(
        !older.has_more,
        "only a deleted message lies beyond this page"
    );
    let three = history_page(app, &c.member, None, 3).await;
    assert_eq!(three.messages.len(), 3);
    assert!(three.has_more);
    assert_eq!(
        history_page(app, &c.member, Some(sent[3].seq), 3)
            .await
            .messages
            .len(),
        1
    );
    assert!(
        !history_page(app, &c.member, Some(sent[3].seq), 3)
            .await
            .has_more
    );

    // The member has read nothing: four messages count, not seven.
    let state = app.state_of(&c.member, app.general).await;
    assert_eq!(state.unread_count, 4);
    // Reading up to g3 leaves g4 and g6, not the deleted g5 between them.
    assert!(
        app.mark_read(&c.member, app.general, sent[3].seq.get())
            .await
            .status()
            .is_success()
    );
    assert_eq!(app.state_of(&c.member, app.general).await.unread_count, 2);
    finish(c).await;
}

#[sqlx::test]
async fn read_positions_survive_deleted_messages(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let first = app.send(&c.author, app.general, "first").await;
    let newest = app.send(&c.author, app.general, "newest").await;
    app.delete(&c.author, newest.id).await;

    // A tombstone is still a position: reading to the end lands on it and cannot go beyond.
    let state: lazy_protocol::UserStateResponse = app
        .mark_read(&c.member, app.general, 1_000_000)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(state.state.last_read, newest.seq);
    assert_eq!(state.state.unread_count, 0);
    let head = app.head().await;
    assert!(
        app.mark_read(&c.member, app.general, first.seq.get())
            .await
            .status()
            .is_success()
    );
    assert_eq!(
        app.state_of(&c.member, app.general).await.last_read,
        newest.seq,
        "never backwards"
    );
    assert_eq!(app.head().await, head);

    // Someone's own read position, set when they sent a message, stays put when it is deleted.
    let mine = app.send(&c.member, app.general, "I wrote this").await;
    assert_eq!(
        app.state_of(&c.member, app.general).await.last_read,
        mine.seq
    );
    app.delete(&c.member, mine.id).await;
    let state = app.state_of(&c.member, app.general).await;
    assert_eq!((state.last_read, state.unread_count), (mine.seq, 0));

    // New messages after the tombstones count as usual.
    app.send(&c.author, app.general, "after").await;
    assert_eq!(app.state_of(&c.member, app.general).await.unread_count, 1);
    finish(c).await;
}

#[sqlx::test]
async fn deleting_removes_the_content_from_everywhere_the_server_kept_it(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app
        .send(&c.author, app.general, "the passphrase is xyzzy")
        .await;
    let edited = app
        .edit(&c.author, &message, "the passphrase is plugh")
        .await;
    app.edit(&c.author, &edited, "the passphrase is quux").await;
    assert_eq!(app.kept_revisions(message.id).await, vec![1, 2]);

    // Every place the words live, counted by looking for them.
    let occurrences = |word: &'static str| {
        let app = &c.app;
        async move {
            let sql = "SELECT \
                (SELECT count(*) FROM messages WHERE document::text LIKE $1 OR plain_text LIKE $1) + \
                (SELECT count(*) FROM message_revisions WHERE document::text LIKE $1) + \
                (SELECT count(*) FROM events WHERE payload::text LIKE $1)";
            let found: i64 = sqlx::query_scalar(sql)
                .bind(format!("%{word}%"))
                .fetch_one(&app.pool)
                .await
                .unwrap();
            found
        }
    };
    for word in ["xyzzy", "plugh", "quux"] {
        assert!(
            occurrences(word).await > 0,
            "{word} should be stored before deletion"
        );
    }

    let tombstone = app.delete(&c.author, message.id).await;
    for word in ["xyzzy", "plugh", "quux"] {
        assert_eq!(
            occurrences(word).await,
            0,
            "{word} should be gone after deletion"
        );
    }
    assert!(app.kept_revisions(message.id).await.is_empty());
    let plain: String = sqlx::query_scalar("SELECT plain_text FROM messages WHERE id = $1")
        .bind(message.id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(plain, "");

    // A device replaying the whole log receives the message only in tombstone form, every time.
    let (_ws, replay, _) = {
        let mut ws = app.connect_ws(&c.member).await;
        ws.subscribe(app.workspace_id, EventSeq::ZERO).await;
        let (events, caught_up) = ws.replay().await;
        (ws, events, caught_up)
    };
    let about_it: Vec<&Message> = replay
        .iter()
        .filter_map(|e| match &e.event {
            Event::MessageCreated { message, .. }
            | Event::MessageEdited { message, .. }
            | Event::MessageDeleted { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(about_it.len(), 4, "created, two edits, deleted");
    assert!(
        about_it
            .iter()
            .all(|m| m.deleted && m.document == deleted_placeholder())
    );
    assert!(
        about_it.iter().all(|m| m.revision == tombstone.revision),
        "a client keeps the highest revision"
    );
    finish(c).await;
}

#[sqlx::test]
async fn a_repeated_edit_after_the_message_was_deleted_gets_the_tombstone(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "secret words").await;
    let operation = OperationId::generate();
    message_of(
        app.edit_raw(
            &c.author,
            message.id,
            operation,
            1,
            "different secret words",
        )
        .await,
    )
    .await;
    let tombstone = app.delete(&c.author, message.id).await;

    // The original result no longer exists anywhere, by design; the tombstone is what remains.
    let again = message_of(
        app.edit_raw(
            &c.author,
            message.id,
            operation,
            1,
            "different secret words",
        )
        .await,
    )
    .await;
    assert_eq!(again, tombstone);
    match app.operation(&c.author, operation).await {
        OperationStatus::EditCommitted { message } => assert_eq!(message, tombstone),
        other => panic!("unexpected status {other:?}"),
    }
    finish(c).await;
}

/// A subscriber who was away while a message was edited and another deleted sees both, in order.
/// A member who was removed meanwhile receives nothing about those messages at all.
#[sqlx::test]
async fn a_subscriber_returning_after_edits_and_deletes_catches_up_and_a_removed_member_does_not(
    pool: PgPool,
) {
    let c = chat(pool).await;
    let app = &c.app;
    let hush = app.create_channel(&c.author, "hush", "private").await;
    assert!(
        app.add_members(&c.author, hush.id, &[&c.member, &c.stranger])
            .await
            .status()
            .is_success()
    );
    let kept = app.send(&c.author, hush.id, "will be edited").await;
    let doomed = app.send(&c.author, hush.id, "will be deleted").await;

    // Two devices of members catch up to here, then disconnect.
    let resume = |who: Account| {
        let app = &c.app;
        async move {
            let mut ws = app.connect_ws(&who).await;
            ws.subscribe(app.workspace_id, EventSeq::ZERO).await;
            let (_events, cursor) = ws.replay().await;
            drop(ws);
            cursor
        }
    };
    let member_cursor = resume(c.member.clone()).await;
    let stranger_cursor = resume(c.stranger.clone()).await;
    let mut live = subscribed(app, &c.author).await;

    // While they are away: an edit, a delete, a new message that is deleted, and the stranger is removed.
    let edited = app.edit(&c.author, &kept, "was edited").await;
    app.delete(&c.author, doomed.id).await;
    let fleeting = app
        .send(&c.author, hush.id, "sent and deleted while away")
        .await;
    app.delete(&c.author, fleeting.id).await;
    assert!(
        app.remove_member(&c.author, hush.id, &c.stranger)
            .await
            .status()
            .is_success()
    );
    app.send(&c.author, hush.id, "after the removal").await;
    let marker = app.register("marker").await;

    // The member who stayed receives everything about the channel's messages, in order.
    let mut ws = app.connect_ws(&c.member).await;
    ws.subscribe(app.workspace_id, member_cursor).await;
    let (replay, _) = ws.replay().await;
    let replay: Vec<EventEnvelope> = replay
        .into_iter()
        .take_while(
            |e| !matches!(&e.event, Event::MemberJoined { user } if user.id == marker.user.id),
        )
        .collect();
    let shapes: Vec<(&str, MessageId)> = message_events(&replay)
        .into_iter()
        .map(|event| match event {
            Event::MessageCreated { message, .. } => ("created", message.id),
            Event::MessageEdited { message, .. } => ("edited", message.id),
            Event::MessageDeleted { message, .. } => ("deleted", message.id),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(shapes[0], ("edited", kept.id));
    assert_eq!(shapes[1], ("deleted", doomed.id));
    assert!(
        shapes.contains(&("created", fleeting.id)) && shapes.contains(&("deleted", fleeting.id))
    );
    assert_eq!(
        shapes.last().map(|s| s.0),
        Some("created"),
        "the message sent after the removal"
    );
    assert!(replay.windows(2).all(|pair| pair[0].seq < pair[1].seq));
    // The edit carries the new revision; the deleted ones arrive only as tombstones.
    let edit_event = replay.iter().find_map(|e| match &e.event {
        Event::MessageEdited { message, .. } => Some(message.clone()),
        _ => None,
    });
    assert_eq!(edit_event, Some(edited));
    for event in message_events(&replay) {
        if let Event::MessageCreated { message, .. } = event
            && message.id == fleeting.id
        {
            assert!(message.deleted, "the log no longer holds what was said");
        }
    }

    // The removed member gets their own removal and not one event about those messages.
    let mut ws = app.connect_ws(&c.stranger).await;
    ws.subscribe(app.workspace_id, stranger_cursor).await;
    let (replay, _) = ws.replay().await;
    assert!(
        message_events(&replay).is_empty(),
        "nothing about messages in the channel they left"
    );
    assert!(replay.iter().any(|e| matches!(&e.event,
        Event::MemberRemoved { conversation_id, user_id }
            if *conversation_id == hush.id && *user_id == c.stranger.user.id)));
    // A device that was connected the whole time saw it live.
    let live_events = live.events_until_marker(marker.user.id).await;
    assert!(message_events(&live_events).len() >= 4);
    finish(c).await;
}

#[sqlx::test]
async fn documents_written_before_formatting_existed_still_read_and_edit(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    // A message exactly as the first versions of the server stored it: a version 1 document.
    let id = MessageId::generate();
    let legacy = r#"{"version":1,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"legacy hello"},{"type":"line_break"},{"type":"text","text":"second line"}]}]}"#;
    let position = app.head().await.get() + 1;
    sqlx::query("UPDATE workspaces SET event_head = $2 WHERE id = $1")
        .bind(app.workspace_id.as_uuid())
        .bind(position)
        .execute(&app.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, author_id, document, plain_text, seq, created_at) \
         VALUES ($1, $2, $3, $4::jsonb, 'legacy hello\nsecond line', $5, now())",
    )
    .bind(id.as_uuid())
    .bind(app.general.as_uuid())
    .bind(c.author.user.id.as_uuid())
    .bind(legacy)
    .bind(position)
    .execute(&app.pool)
    .await
    .unwrap();

    // It is served, as history and on its own.
    let page = app.history_ok(&c.member, app.general, &[]).await;
    assert_eq!(page.messages.len(), 1);
    assert_eq!(
        page.messages[0].document.plain_text(),
        "legacy hello\nsecond line"
    );
    assert_eq!(app.get_message(&c.member, id).await, page.messages[0]);
    assert_eq!(
        (
            page.messages[0].revision,
            page.messages[0].edited_at,
            page.messages[0].deleted
        ),
        (1, None, false)
    );

    // It can be edited; the original is kept exactly as it was stored.
    let edited = app
        .edit(&c.author, &page.messages[0], "now *formatted*")
        .await;
    assert_eq!(edited.revision, 2);
    let kept: String =
        sqlx::query_scalar("SELECT document::text FROM message_revisions WHERE message_id = $1")
            .bind(id.as_uuid())
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert!(kept.contains("legacy hello"));

    // A send whose acknowledgment was lost across the upgrade is recognized when retried: its
    // receipt was written with the version 1 fingerprint, and it must not conflict with itself.
    let resent_operation = OperationId::generate();
    let document = MessageDocument::from_plain_text("sent before the upgrade").unwrap();
    let [_, legacy_fingerprint] =
        lazy_server::crypto::request_fingerprints(app.general, &document).unwrap();
    let before = app
        .send(&c.author, app.general, "stand-in for the original send")
        .await;
    sqlx::query(
        "INSERT INTO operation_receipts (user_id, operation_id, request_hash, kind, message_id) \
         VALUES ($1, $2, $3, 'send_message', $4)",
    )
    .bind(c.author.user.id.as_uuid())
    .bind(resent_operation.as_uuid())
    .bind(&legacy_fingerprint[..])
    .bind(before.id.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();
    let retried = app
        .send_raw(
            &c.author,
            app.general,
            resent_operation,
            "sent before the upgrade",
        )
        .await;
    assert_eq!(message_of_send(retried).await.id, before.id);
    denied(
        app.send_raw(&c.author, app.general, resent_operation, "different words")
            .await,
        ErrorCode::Conflict,
    )
    .await;
    finish(c).await;
}

async fn message_of_send(response: reqwest::Response) -> Message {
    assert!(
        response.status().is_success(),
        "status {}",
        response.status()
    );
    response
        .json::<lazy_protocol::SendMessageResponse>()
        .await
        .unwrap()
        .message
}

#[sqlx::test]
async fn the_server_reports_the_current_protocol_version(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
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
    assert_eq!(
        app.bootstrap_ok(&c.author).await.protocol_version,
        PROTOCOL_VERSION
    );
    finish(c).await;
}

// --- Races ------------------------------------------------------------------------------------

/// Many editors start from the same revision: exactly one wins and the rest are told it is stale.
#[sqlx::test]
async fn editors_racing_from_one_revision_commit_exactly_one(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "start").await;
    let baseline = app.head().await;

    let attempts = (0..16).map(|n| {
        let http = app.http.clone();
        let url = app.url(&lazy_protocol::api::paths::message(message.id));
        let token = c.author.token.clone();
        tokio::spawn(async move {
            let body = json!({
                "operation_id": OperationId::generate(), "base_revision": 1,
                "document": MessageDocument::from_markdown(&format!("edit {n}")).unwrap(),
            });
            http.patch(url)
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        })
    });
    let statuses = tokio::time::timeout(DEADLINE, join_all(attempts))
        .await
        .expect("the race finishes");
    let statuses: Vec<u16> = statuses.into_iter().map(|s| s.unwrap()).collect();
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        1,
        "{statuses:?}"
    );
    assert!(
        statuses.iter().all(|s| *s == 200 || *s == 409),
        "{statuses:?}"
    );

    assert_eq!(app.get_message(&c.author, message.id).await.revision, 2);
    assert_eq!(app.kept_revisions(message.id).await, vec![1]);
    let edits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM events WHERE kind = 'message_edited' AND seq > $1",
    )
    .bind(baseline.get())
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(edits, 1);
    finish(c).await;
}

/// Editors that refetch and retry whenever they are stale all eventually commit, every revision
/// exists exactly once, and the log lists them in revision order.
#[sqlx::test]
async fn editors_that_retry_when_stale_lose_nothing(pool: PgPool) {
    const EDITORS: usize = 6;
    const EDITS_EACH: usize = 6;
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "start").await;

    let editors = (0..EDITORS).map(|editor| {
        let http = app.http.clone();
        let url = app.url(&lazy_protocol::api::paths::message(message.id));
        let token = c.author.token.clone();
        tokio::spawn(async move {
            for n in 0..EDITS_EACH {
                let mut attempts = 0;
                loop {
                    attempts += 1;
                    assert!(attempts < 500, "an editor starved");
                    let current: MessageResponse =
                        http.get(&url).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
                    let body = json!({
                        "operation_id": OperationId::generate(),
                        "base_revision": current.message.revision,
                        "document": MessageDocument::from_markdown(&format!("editor {editor} edit {n}")).unwrap(),
                    });
                    let response = http.patch(&url).bearer_auth(&token).json(&body).send().await.unwrap();
                    match response.status().as_u16() {
                        200 => break,
                        409 => continue,
                        other => panic!("unexpected status {other}"),
                    }
                }
            }
        })
    });
    for result in tokio::time::timeout(DEADLINE, join_all(editors))
        .await
        .expect("the editors finish")
    {
        result.unwrap();
    }

    let total = (EDITORS * EDITS_EACH) as u32;
    assert_eq!(
        app.get_message(&c.author, message.id).await.revision,
        total + 1
    );
    assert_eq!(
        app.kept_revisions(message.id).await,
        (1..=total as i32).collect::<Vec<_>>()
    );
    let revisions_in_log: Vec<i32> = sqlx::query_scalar(
        "SELECT (payload -> 'message' ->> 'revision')::int FROM events \
         WHERE message_id = $1 AND kind = 'message_edited' ORDER BY seq",
    )
    .bind(message.id.as_uuid())
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(
        revisions_in_log,
        (2..=total as i32 + 1).collect::<Vec<_>>(),
        "each revision once, in order"
    );
    finish(c).await;
}

/// Edits race a delete. Whatever the interleaving: the delete wins, every edit that was accepted
/// is ordered before it in the log and counted in the tombstone's revision, the rest are refused,
/// and nothing of the content remains.
#[sqlx::test]
async fn edits_racing_a_delete_are_ordered_before_it_or_refused(pool: PgPool) {
    const ROUNDS: usize = 12;
    const EDITORS: usize = 5;
    let c = chat(pool).await;
    let app = &c.app;

    for round in 0..ROUNDS {
        let message = app
            .send(&c.author, app.general, &format!("round {round} original"))
            .await;
        let url = app.url(&lazy_protocol::api::paths::message(message.id));

        let deleter = {
            let http = app.http.clone();
            let (url, token) = (url.clone(), c.author.token.clone());
            tokio::spawn(async move {
                http.delete(url)
                    .bearer_auth(token)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            })
        };
        let editors = (0..EDITORS).map(|editor| {
            let http = app.http.clone();
            let (url, token) = (url.clone(), c.author.token.clone());
            tokio::spawn(async move {
                let mut accepted = 0u32;
                for n in 0..4 {
                    let current = http.get(&url).bearer_auth(&token).send().await.unwrap();
                    let current: MessageResponse = current.json().await.unwrap();
                    if current.message.deleted {
                        break;
                    }
                    let body = json!({
                        "operation_id": OperationId::generate(),
                        "base_revision": current.message.revision,
                        "document": MessageDocument::from_markdown(&format!("round {round} editor {editor} edit {n}")).unwrap(),
                    });
                    match http.patch(&url).bearer_auth(&token).json(&body).send().await.unwrap().status().as_u16() {
                        200 => accepted += 1,
                        404 | 409 => {}
                        other => panic!("unexpected status {other}"),
                    }
                }
                accepted
            })
        });
        let (deleted, accepted) = tokio::time::timeout(DEADLINE, async {
            let deleted = deleter.await.unwrap();
            let accepted: u32 = join_all(editors)
                .await
                .into_iter()
                .map(|a| a.unwrap())
                .sum();
            (deleted, accepted)
        })
        .await
        .expect("the round finishes");
        assert_eq!(deleted, 200);

        let tombstone = app.get_message(&c.author, message.id).await;
        assert!(tombstone.deleted);
        assert_eq!(
            tombstone.revision,
            1 + accepted + 1,
            "round {round}: edits accepted plus the deletion"
        );
        assert!(app.kept_revisions(message.id).await.is_empty());

        let events = app.message_events(message.id).await;
        let deleted_at = events
            .iter()
            .find(|(_, kind)| kind == "message_deleted")
            .map(|(seq, _)| *seq)
            .unwrap();
        let edits: Vec<i64> = events
            .iter()
            .filter(|(_, kind)| kind == "message_edited")
            .map(|(seq, _)| *seq)
            .collect();
        assert_eq!(
            edits.len() as u32,
            accepted,
            "every accepted edit is in the log"
        );
        assert!(
            edits.iter().all(|seq| *seq < deleted_at),
            "nothing is edited after it was deleted"
        );
        let live_content: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM events WHERE message_id = $1 AND kind IN ('message_created', 'message_edited') \
             AND (payload -> 'message' ->> 'deleted') IS DISTINCT FROM 'true'",
        )
        .bind(message.id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
        assert_eq!(
            live_content, 0,
            "no event keeps the text of a deleted message"
        );
    }
    finish(c).await;
}

/// Simultaneous deletes, by the author and a moderator, produce one tombstone and one event.
#[sqlx::test]
async fn simultaneous_deletes_commit_once(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "going").await;
    let url = app.url(&lazy_protocol::api::paths::message(message.id));

    let attempts = (0..8).map(|n| {
        let http = app.http.clone();
        let url = url.clone();
        let token = if n % 2 == 0 {
            c.author.token.clone()
        } else {
            c.admin.token.clone()
        };
        tokio::spawn(async move {
            let response = http.delete(url).bearer_auth(token).send().await.unwrap();
            assert!(response.status().is_success(), "{}", response.status());
            response.json::<MessageResponse>().await.unwrap().message
        })
    });
    let results = tokio::time::timeout(DEADLINE, join_all(attempts))
        .await
        .expect("the deletes finish");
    let results: Vec<Message> = results.into_iter().map(|m| m.unwrap()).collect();
    assert!(
        results.windows(2).all(|pair| pair[0] == pair[1]),
        "everyone gets the same tombstone"
    );
    let kinds: Vec<String> = app
        .message_events(message.id)
        .await
        .into_iter()
        .map(|(_, k)| k)
        .collect();
    assert_eq!(kinds, vec!["message_created", "message_deleted"]);
    assert_eq!(results[0].revision, 2);
    finish(c).await;
}
