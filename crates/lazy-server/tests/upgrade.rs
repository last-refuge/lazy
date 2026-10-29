//! Upgrading in place: a database left by an earlier release, with edited, deleted, and reacted-to
//! messages, an idempotency receipt, and a log of events in their old shapes, takes the migrations
//! since and then works with everything new. One test starts from migrations 0001 to 0005 (protocol
//! version 4) and one from 0006 (protocol 5, with mentions and a group), and each ends with the
//! newest features (mentions, groups, threads) used on top of the old rows.

mod common;

use common::*;
use lazy_domain::mention::MentionTarget;
use lazy_domain::{
    ConversationId, DisplayName, EventSeq, MessageId, OperationId, Username, WorkspaceId,
};
use lazy_protocol::{ErrorCode, Event, Mentions, OperationStatus, User};
use lazy_server::{PasswordCost, RetentionPolicy, ServerOptions, db, testing};
use sqlx::PgPool;

/// The release before the previous one's data, in the shapes it wrote them.
const POPULATED: &str = include_str!("fixtures/populated_0005.sql");
/// What the previous release (mentions and groups, migration 0006) added to it.
const POPULATED_0006: &str = include_str!("fixtures/populated_0006.sql");
/// What the release after that (threads, migration 0007) added.
const POPULATED_0007: &str = include_str!("fixtures/populated_0007.sql");
/// What the one after that (webhooks, migration 0008) added.
const POPULATED_0008: &str = include_str!("fixtures/populated_0008.sql");
/// What the one after that (attachments, migration 0009) added.
const POPULATED_0009: &str = include_str!("fixtures/populated_0009.sql");
/// What the one after that (custom emoji, migration 0010) added.
const POPULATED_0010: &str = include_str!("fixtures/populated_0010.sql");

/// What the administration release (migration 0014) added: settings, a deactivated person, and
/// invitations.
const POPULATED_0014: &str = include_str!("fixtures/populated_0014.sql");

fn id<T: std::str::FromStr>(suffix: &str) -> T
where
    T::Err: std::fmt::Debug,
{
    format!("01a00000-0000-7000-8000-{suffix:0>12}")
        .parse()
        .expect("a fixture id")
}

fn account(name: &str, suffix: &str, token_digit: char) -> Account {
    Account {
        token: format!("lzy_{}", token_digit.to_string().repeat(64)),
        user: User {
            id: id(suffix),
            username: Username::parse(name).unwrap(),
            display_name: DisplayName::parse(name).unwrap(),
            is_bot: false,
            status: None,
            deactivated: false,
            avatar_version: 0,
        },
    }
}

fn user(account: &Account) -> MentionTarget {
    MentionTarget::User {
        id: account.user.id,
    }
}

#[sqlx::test(migrations = false)]
async fn a_database_left_by_the_previous_release_upgrades_in_place(pool: PgPool) {
    // The previous release's schema, filled the way it filled it.
    sqlx::migrate!("./migrations")
        .run_to(5, &pool)
        .await
        .expect("migrations 0001 to 0005");
    sqlx::raw_sql(POPULATED)
        .execute(&pool)
        .await
        .expect("the populated database");

    // This release starts: its migration applies on top, and applying it again is a no-op.
    db::migrate(&pool).await.expect("migration 0006");
    db::migrate(&pool)
        .await
        .expect("a second run changes nothing");
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(applied, (1..=15).collect::<Vec<i64>>());

    let alice = account("alice", "1", 'a');
    let bob = account("bob", "2", 'b');
    let carol = account("carol", "3", 'c');
    let workspace_id: WorkspaceId = id("a0");
    let general: ConversationId = id("c1");
    let (m1, m2, m3): (MessageId, MessageId, MessageId) = (id("f1"), id("f2"), id("f3"));
    let server = testing::spawn_test_server_with(
        pool.clone(),
        ServerOptions {
            open_registration: true,
            open_workspace_creation: true,
            password_cost: PasswordCost::FAST_FOR_TESTS,
            retention: RetentionPolicy::disabled(),
        },
    )
    .await;
    let app = App {
        server,
        pool: pool.clone(),
        http: reqwest::Client::new(),
        workspace_id,
        general,
    };

    // Old sessions still work, and nothing is mentioned, grouped, or counted that was not before.
    let boot = app.bootstrap_ok(&bob).await;
    assert_eq!(boot.protocol_version, lazy_protocol::PROTOCOL_VERSION);
    assert!(boot.workspaces[0].groups.is_empty());
    let state = app.state_of(&bob, general).await;
    assert_eq!((state.unread_count, state.mention_count), (2, 0));
    let state = app.state_of(&carol, general).await;
    assert_eq!((state.unread_count, state.mention_count), (0, 0));

    // History is exactly what it was: the edit, the reaction, an ordinary message (the deleted one
    // is not listed, and is still a tombstone when asked for).
    let page = app.history_ok(&bob, general, &[]).await;
    let mut messages = page.messages.clone();
    messages.sort_by_key(|m| m.seq);
    assert_eq!(
        messages.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![m1, m3]
    );
    assert!(messages.iter().all(|m| m.mentions.is_empty()));
    let edited = &messages[0];
    assert_eq!((edited.revision, edited.deleted), (2, false));
    assert_eq!(edited.document.plain_text(), "hello (edited)");
    assert!(edited.edited_at.is_some());
    assert_eq!(edited.reactions.len(), 1);
    assert_eq!(edited.reactions[0].users, vec![bob.user.id]);
    assert_eq!(edited.reactions_as_of.get(), 6);
    assert_eq!(messages[1].document.plain_text(), "plain message");
    let tombstone = app.get_message(&bob, m2).await;
    assert!(tombstone.deleted && tombstone.reactions.is_empty() && tombstone.mentions.is_empty());

    // The old receipt still answers.
    match app.operation(&alice, id::<OperationId>("d1")).await {
        OperationStatus::Committed { message } => assert_eq!(message.id, m1),
        other => panic!("unexpected status {other:?}"),
    }

    // Replaying the whole old log parses every event it holds, in order, with nothing to undo.
    let mut ws = app.connect_ws(&carol).await;
    ws.subscribe(workspace_id, EventSeq::ZERO).await;
    let (events, head) = ws.replay().await;
    assert_eq!(head.get(), 9);
    assert_eq!(
        events.iter().map(|e| e.seq.get()).collect::<Vec<_>>(),
        (1..=9).collect::<Vec<_>>()
    );
    for envelope in &events {
        match &envelope.event {
            Event::MessageEdited {
                previous_mentions, ..
            } => assert!(previous_mentions.is_empty()),
            Event::MessageDeleted { had_mentions, .. } => assert!(had_mentions.is_empty()),
            _ => {}
        }
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.event, Event::MessageDeleted { message, .. } if message.id == m2))
    );

    // Everything new works on top of the old data. An old message can be edited to mention
    // someone: Bob has read nothing, so it counts for him.
    let old = app.get_message(&alice, m1).await;
    let mentioning = app
        .edit_mentioning(&alice, &old, "hello @bob", &[("bob", user(&bob))])
        .await;
    assert_eq!(mentioning.mentions.users, vec![bob.user.id]);
    assert_eq!(app.state_of(&bob, general).await.mention_count, 1);

    let team = app.create_group(&alice, "team", &[&bob, &carol]).await;
    assert_eq!(app.list_groups(&bob).await, vec![team.clone()]);
    let to_team = app
        .send_mentioning(
            &carol,
            general,
            "@team standup",
            &[("team", MentionTarget::Group { id: team.id })],
        )
        .await;
    assert_eq!(to_team.mentions.users, vec![bob.user.id]);
    assert_eq!(app.state_of(&bob, general).await.mention_count, 2);

    // Deleting it turns its old events into tombstones and stops it counting; the old edit event
    // replaced nobody, and the deletion says who the last version mentioned.
    app.delete(&alice, m1).await;
    assert_eq!(app.state_of(&bob, general).await.mention_count, 1);
    let (replaced, had): (Option<serde_json::Value>, serde_json::Value) = sqlx::query_as(
        "SELECT \
             (SELECT payload -> 'previous_mentions' FROM events \
              WHERE message_id = $1 AND kind = 'message_edited' ORDER BY seq LIMIT 1), \
             (SELECT payload -> 'had_mentions' FROM events \
              WHERE message_id = $1 AND kind = 'message_deleted')",
    )
    .bind(m1.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(replaced, None, "the old edit replaced nobody");
    assert_eq!(
        serde_json::from_value::<Mentions>(had).unwrap().users,
        vec![bob.user.id]
    );

    // A new send mentioning a person counts for them (Carol read everything before it).
    let to_carol = app
        .send_mentioning(&bob, general, "@carol hello", &[("carol", user(&carol))])
        .await;
    assert_eq!(to_carol.mentions.users, vec![carol.user.id]);
    assert_eq!(app.state_of(&carol, general).await.mention_count, 1);

    // Reactions, edits, and sends on the old rows keep working.
    let reacted = app.react(&carol, m3, "🎉", true).await;
    assert_eq!(reacted.reactions.len(), 1);
    let m3_now = app.get_message(&carol, m3).await;
    let edited = app.edit(&carol, &m3_now, "plain message, edited").await;
    assert_eq!(edited.revision, 2);

    // The handle namespace covers the people who were already here.
    let response = app
        .create_group_raw(&alice, "carol", "Carol", &[], OperationId::generate())
        .await;
    let (status, error) = api_error(response).await;
    assert_error(status, &error, ErrorCode::NameTaken);
    let response = app.try_register("team", PASSWORD).await;
    let (status, error) = api_error(response).await;
    assert_error(status, &error, ErrorCode::UsernameTaken);

    app.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn a_database_left_by_the_mentions_release_upgrades_in_place_and_gains_threads(pool: PgPool) {
    // The release before it, then its own migration and its data.
    sqlx::migrate!("./migrations")
        .run_to(5, &pool)
        .await
        .expect("migrations 0001 to 0005");
    sqlx::raw_sql(POPULATED)
        .execute(&pool)
        .await
        .expect("the 0005 data");
    sqlx::migrate!("./migrations")
        .run_to(6, &pool)
        .await
        .expect("migration 0006");
    sqlx::raw_sql(POPULATED_0006)
        .execute(&pool)
        .await
        .expect("the 0006 data");
    let head_before: i64 = sqlx::query_scalar("SELECT event_head FROM workspaces")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(head_before, 12);

    // This release starts: migration 0007 applies on top.
    db::migrate(&pool).await.expect("migration 0007");
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(applied, (1..=15).collect::<Vec<i64>>());
    // Every message is still listed except the one that was deleted; nothing has a thread; the
    // mention rows are listed; the indexes the counts and pages use exist.
    let unlisted: Vec<uuid::Uuid> = sqlx::query_scalar("SELECT id FROM messages WHERE NOT listed")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(unlisted, vec![id::<MessageId>("f2").as_uuid()]);
    let (threads, states): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM threads), (SELECT count(*) FROM thread_user_state)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((threads, states), (0, 0));
    let unlisted_mentions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM message_mentions WHERE NOT listed")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(unlisted_mentions, 0);
    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT indexname FROM pg_indexes WHERE indexname IN \
         ('messages_history_idx', 'messages_replies_idx', 'messages_channel_mentions_idx', 'message_mentions_user_idx') \
         ORDER BY indexname",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(indexes.len(), 4);

    let alice = account("alice", "1", 'a');
    let bob = account("bob", "2", 'b');
    let carol = account("carol", "3", 'c');
    let workspace_id: WorkspaceId = id("a0");
    let general: ConversationId = id("c1");
    let [m1, m2, m3, m4, m5]: [MessageId; 5] = ["f1", "f2", "f3", "f4", "f5"].map(id);
    let server = testing::spawn_test_server_with(
        pool.clone(),
        ServerOptions {
            open_registration: true,
            open_workspace_creation: true,
            password_cost: PasswordCost::FAST_FOR_TESTS,
            retention: RetentionPolicy::disabled(),
        },
    )
    .await;
    let app = App {
        server,
        pool: pool.clone(),
        http: reqwest::Client::new(),
        workspace_id,
        general,
    };

    // The counts are what they were: replies change nothing for rows that have none.
    let counts =
        |state: lazy_protocol::ConversationUserState| (state.unread_count, state.mention_count);
    assert_eq!(counts(app.state_of(&carol, general).await), (2, 2));
    assert_eq!(counts(app.state_of(&bob, general).await), (3, 1));
    assert_eq!(counts(app.state_of(&alice, general).await), (1, 0));
    let boot = app.bootstrap_ok(&bob).await;
    assert!(boot.workspaces[0].threads.is_empty());
    assert_eq!(boot.workspaces[0].groups.len(), 1);
    // History lists the same four messages, none of them a reply and none with a thread.
    let page = app.history_ok(&bob, general, &[]).await;
    let listed: Vec<MessageId> = page.messages.iter().map(|m| m.id).collect();
    assert_eq!(listed, vec![m1, m3, m4, m5]);
    assert!(
        page.messages
            .iter()
            .all(|m| m.thread_root.is_none() && m.thread.is_none() && !m.in_channel)
    );
    assert_eq!(page.messages[2].mentions.users, vec![carol.user.id]);
    assert!(page.messages[3].mentions.channel);
    // The whole old log replays, in order.
    let mut ws = app.connect_ws(&carol).await;
    ws.subscribe(workspace_id, EventSeq::ZERO).await;
    let (events, head) = ws.replay().await;
    assert_eq!((head.get(), events.len()), (12, 12));

    // Threads on top of the old rows. Bob replies to alice's old, edited, reacted-to message,
    // naming carol; the reply stays in the thread, so no conversation count moves.
    let root = app.get_message(&bob, m1).await;
    assert_eq!(root.reactions.len(), 1);
    let named = [("carol", MentionTarget::User { id: carol.user.id })];
    let reply = app
        .reply_mentioning(&bob, &root, "@carol see this", &named, false)
        .await;
    let summary = app.get_message(&alice, m1).await.thread.expect("a summary");
    assert_eq!(
        (summary.reply_count, summary.last_reply_seq),
        (1, reply.seq)
    );
    assert!(summary.as_of.get() > 12);
    assert_eq!(counts(app.state_of(&carol, general).await), (2, 2));
    let carol_thread = app
        .thread_state(&carol, m1)
        .await
        .expect("named, so following");
    assert_eq!(
        (carol_thread.unread_count, carol_thread.mention_count),
        (1, 1)
    );
    let alice_thread = app
        .thread_state(&alice, m1)
        .await
        .expect("the author follows");
    assert_eq!(
        (alice_thread.unread_count, alice_thread.mention_count),
        (1, 0)
    );
    // The same reply, also sent to the channel, counts there.
    let loud = app
        .reply_in_channel(&bob, &root, "and in the channel")
        .await;
    assert_eq!(counts(app.state_of(&carol, general).await), (3, 2));
    assert!(loud.in_channel);
    // Replying to the old @channel message and then deleting it leaves a tombstone with its
    // thread, no longer counted by anyone.
    let standup = app.get_message(&bob, m5).await;
    app.reply(&carol, &standup, "on my way").await;
    assert_eq!(counts(app.state_of(&bob, general).await), (3, 1));
    let tombstone = app.delete(&alice, m5).await;
    assert!(tombstone.deleted && tombstone.thread.as_ref().map(|t| t.reply_count) == Some(1));
    assert_eq!(counts(app.state_of(&bob, general).await), (2, 0));
    let page = app.history_ok(&bob, general, &[]).await;
    assert!(
        page.messages
            .iter()
            .any(|m| m.id == m5 && m.deleted && m.thread.is_some())
    );
    // The old tombstone (m2) never had replies and is still not listed.
    assert!(page.messages.iter().all(|m| m.id != m2));
    // Mentions and groups still work on old rows.
    let team = app.list_groups(&bob).await;
    let to_team = app
        .send_mentioning(
            &alice,
            general,
            "@team hello",
            &[("team", MentionTarget::Group { id: team[0].id })],
        )
        .await;
    assert_eq!(to_team.mentions.users, vec![bob.user.id, carol.user.id]);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn a_database_left_by_the_threads_release_upgrades_in_place_and_gains_webhooks(pool: PgPool) {
    // Each release's migrations, and the data it left, in order.
    sqlx::migrate!("./migrations")
        .run_to(5, &pool)
        .await
        .expect("migrations 0001 to 0005");
    sqlx::raw_sql(POPULATED)
        .execute(&pool)
        .await
        .expect("the 0005 data");
    sqlx::migrate!("./migrations")
        .run_to(6, &pool)
        .await
        .expect("migration 0006");
    sqlx::raw_sql(POPULATED_0006)
        .execute(&pool)
        .await
        .expect("the 0006 data");
    sqlx::migrate!("./migrations")
        .run_to(7, &pool)
        .await
        .expect("migration 0007");
    sqlx::raw_sql(POPULATED_0007)
        .execute(&pool)
        .await
        .expect("the 0007 data");
    let head_before: i64 = sqlx::query_scalar("SELECT event_head FROM workspaces")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(head_before, 16);

    // This release starts: migration 0008 applies on top.
    db::migrate(&pool).await.expect("migration 0008");
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(applied, (1..=15).collect::<Vec<i64>>());
    // Everyone who was here is a person, with the password they had and the session they had; there
    // are no webhooks.
    let (people, bots, sessions, hooks): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM users WHERE NOT is_bot), (SELECT count(*) FROM users WHERE is_bot), \
                (SELECT count(*) FROM sessions WHERE NOT user_is_bot), (SELECT count(*) FROM webhooks)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((people, bots, sessions, hooks), (3, 0, 3, 0));

    let alice = account("alice", "1", 'a');
    let bob = account("bob", "2", 'b');
    let carol = account("carol", "3", 'c');
    let workspace_id: WorkspaceId = id("a0");
    let general: ConversationId = id("c1");
    let root: MessageId = id("f1");
    let server = testing::spawn_test_server_with(
        pool.clone(),
        ServerOptions {
            open_registration: true,
            open_workspace_creation: true,
            password_cost: PasswordCost::FAST_FOR_TESTS,
            retention: RetentionPolicy::disabled(),
        },
    )
    .await;
    let app = App {
        server,
        pool: pool.clone(),
        http: reqwest::Client::new(),
        workspace_id,
        general,
    };

    // The old sessions work; nobody is a bot on the wire; the counts, the thread and its states are
    // what they were.
    let boot = app.bootstrap_ok(&alice).await;
    assert!(boot.workspaces[0].members.iter().all(|user| !user.is_bot));
    assert!(!boot.user.is_bot);
    let counts =
        |state: lazy_protocol::ConversationUserState| (state.unread_count, state.mention_count);
    assert_eq!(counts(app.state_of(&alice, general).await), (2, 0));
    assert_eq!(counts(app.state_of(&bob, general).await), (4, 1));
    assert_eq!(counts(app.state_of(&carol, general).await), (2, 2));
    let thread = |state: Option<lazy_protocol::ThreadUserState>| {
        state.map(|s| (s.following, s.unread_count, s.mention_count))
    };
    assert_eq!(
        thread(app.thread_state(&alice, root).await),
        Some((true, 2, 0))
    );
    assert_eq!(
        thread(app.thread_state(&bob, root).await),
        Some((true, 1, 0))
    );
    assert_eq!(
        thread(app.thread_state(&carol, root).await),
        Some((true, 0, 0))
    );
    let summary = app
        .get_message(&bob, root)
        .await
        .thread
        .expect("the thread survived");
    assert_eq!((summary.reply_count, summary.as_of.get()), (2, 16));
    // The whole old log replays.
    let mut ws = app.connect_ws(&carol).await;
    ws.subscribe(workspace_id, EventSeq::ZERO).await;
    let (events, head) = ws.replay().await;
    assert_eq!((head.get(), events.len()), (16, 16));

    // Webhooks on top of all of it. Alice owns the workspace.
    let hook = app.create_webhook(&alice, general, "Deploys").await;
    let bot = hook.webhook.bot_user_id;
    let first = app.deliver(&hook, "deployed :rocket:").await;
    let message = app.get_message(&bob, first).await;
    assert_eq!(message.author_id, bot);
    assert!(message.document.plain_text().contains('🚀'));
    assert_eq!(counts(app.state_of(&alice, general).await), (3, 0));
    assert_eq!(counts(app.state_of(&bob, general).await), (5, 1));
    assert_eq!(counts(app.state_of(&carol, general).await), (3, 2));
    // People can answer it in a thread, as under any message, and react to it.
    let reply = app.reply(&bob, &message, "thanks").await;
    assert_eq!(reply.thread_root, Some(first));
    assert_eq!(
        app.react(&carol, first, "👍", true).await.reactions.len(),
        1
    );
    // The bot is in the member list, marked, and the old thread is untouched by it.
    let boot = app.bootstrap_ok(&alice).await;
    let marked: Vec<lazy_domain::UserId> = boot.workspaces[0]
        .members
        .iter()
        .filter(|user| user.is_bot)
        .map(|user| user.id)
        .collect();
    assert_eq!(marked, vec![bot]);
    assert_eq!(
        app.get_message(&bob, root)
            .await
            .thread
            .unwrap()
            .reply_count,
        2
    );
    // Revoking removes the bot and keeps its message with it as the author.
    assert_eq!(
        app.delete_webhook_raw(&alice, hook.webhook.id)
            .await
            .status()
            .as_u16(),
        204
    );
    assert_eq!(app.get_message(&bob, first).await.author_id, bot);
    assert!(
        app.deliver_raw(&hook.url, "again", &[])
            .await
            .status()
            .is_client_error()
    );
    // Old people still sign in as they did (their stored hash is the placeholder the fixtures use,
    // so only a bad password is refused here; the point is that nothing about it changed).
    let (status, error) = api_error(app.try_login("alice", "not the password").await).await;
    assert_error(status, &error, ErrorCode::InvalidCredentials);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn a_database_left_by_the_webhooks_release_upgrades_in_place_and_gains_attachments(
    pool: PgPool,
) {
    // Each release's migrations, and the data it left, in order.
    sqlx::migrate!("./migrations")
        .run_to(5, &pool)
        .await
        .expect("0001 to 0005");
    sqlx::raw_sql(POPULATED)
        .execute(&pool)
        .await
        .expect("the 0005 data");
    sqlx::migrate!("./migrations")
        .run_to(6, &pool)
        .await
        .expect("0006");
    sqlx::raw_sql(POPULATED_0006)
        .execute(&pool)
        .await
        .expect("the 0006 data");
    sqlx::migrate!("./migrations")
        .run_to(7, &pool)
        .await
        .expect("0007");
    sqlx::raw_sql(POPULATED_0007)
        .execute(&pool)
        .await
        .expect("the 0007 data");
    sqlx::migrate!("./migrations")
        .run_to(8, &pool)
        .await
        .expect("0008");
    sqlx::raw_sql(POPULATED_0008)
        .execute(&pool)
        .await
        .expect("the 0008 data");
    let head_before: i64 = sqlx::query_scalar("SELECT event_head FROM workspaces")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(head_before, 19);

    // This release starts: migrations 0009 and 0010 apply on top.
    db::migrate(&pool).await.expect("migrations 0009 and 0010");
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(applied, (1..=15).collect::<Vec<i64>>());
    let files: i64 = sqlx::query_scalar("SELECT count(*) FROM attachments")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(files, 0);

    let alice = account("alice", "1", 'a');
    let bob = account("bob", "2", 'b');
    let carol = account("carol", "3", 'c');
    let workspace_id: WorkspaceId = id("a0");
    let general: ConversationId = id("c1");
    let bot_message: MessageId = id("f8");
    let server = testing::spawn_test_server_with(
        pool.clone(),
        ServerOptions {
            open_registration: true,
            open_workspace_creation: true,
            password_cost: PasswordCost::FAST_FOR_TESTS,
            retention: RetentionPolicy::disabled(),
        },
    )
    .await;
    let app = App {
        server,
        pool: pool.clone(),
        http: reqwest::Client::new(),
        workspace_id,
        general,
    };

    // Everything that was there is there, with no files on it, and the old log replays.
    let page = app.history_ok(&bob, general, &[]).await;
    assert!(page.messages.iter().all(|m| m.attachments.is_empty()));
    assert!(page.messages.iter().any(|m| m.id == bot_message));
    let mut ws = app.connect_ws(&carol).await;
    ws.subscribe(workspace_id, EventSeq::ZERO).await;
    let (events, head) = ws.replay().await;
    assert_eq!((head.get(), events.len()), (19, 19));

    // Files on top of the old rows: an old message gets a reply with a file, a new message has an
    // image and a note, an old webhook still delivers, and a moderator removes a bot's message.
    let old = app.get_message(&bob, id("f1")).await;
    let note = app
        .upload(
            &alice,
            general,
            b"a note".to_vec(),
            "text/plain",
            "note.txt",
        )
        .await;
    let reply = app
        .send_with_files_raw(
            &alice,
            general,
            lazy_domain::OperationId::generate(),
            "",
            &[note.id],
            Some(&old),
            false,
        )
        .await;
    let reply = reply
        .json::<lazy_protocol::SendMessageResponse>()
        .await
        .unwrap()
        .message;
    assert_eq!(
        (reply.thread_root, reply.attachments.len()),
        (Some(old.id), 1)
    );
    assert_eq!(
        app.get_message(&bob, old.id)
            .await
            .thread
            .unwrap()
            .reply_count,
        3
    );
    let picture = app
        .upload(
            &bob,
            general,
            png_bytes(900, 300, 255),
            "image/png",
            "chart.png",
        )
        .await;
    assert!(picture.has_thumbnail);
    let sent = app
        .send_with_files(&bob, general, "a chart", &[picture.id])
        .await;
    assert_eq!(
        app.get_message(&carol, sent.id).await.attachments,
        vec![picture.clone()]
    );
    let hooked = app
        .http
        .post(app.url(&format!(
            "/hooks/{}/{}",
            id::<lazy_domain::WebhookId>("b9"),
            "d".repeat(64)
        )))
        .json(&serde_json::json!({"text": "deployed v2"}))
        .send()
        .await
        .unwrap();
    assert_eq!(hooked.status().as_u16(), 200);
    assert_eq!(
        app.download_raw(&carol, picture.id, true, false, &[])
            .await
            .status()
            .as_u16(),
        200
    );
    assert!(app.delete(&alice, bot_message).await.deleted);
    // The cleanup job copes with a database that has both old and new rows.
    let store =
        lazy_server::attachments::store::FileStore::open_dir(app.server.attachments_dir()).unwrap();
    let report =
        lazy_server::attachments::cleanup(&pool, &store, std::time::Duration::from_secs(24 * 3600))
            .await
            .unwrap();
    assert_eq!(report, Default::default());
    app.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn a_database_left_by_the_attachments_release_upgrades_in_place_and_gains_custom_emoji(
    pool: PgPool,
) {
    // Each release's migrations, and the data it left, in order, up to the attachments release.
    for (version, data) in [
        (5, Some(POPULATED)),
        (6, Some(POPULATED_0006)),
        (7, Some(POPULATED_0007)),
        (8, Some(POPULATED_0008)),
        (9, Some(POPULATED_0009)),
    ] {
        sqlx::migrate!("./migrations")
            .run_to(version, &pool)
            .await
            .unwrap_or_else(|e| panic!("migrations up to {version}: {e}"));
        if let Some(data) = data {
            sqlx::raw_sql(data)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("the data of {version}: {e}"));
        }
    }
    let head_before: i64 = sqlx::query_scalar("SELECT event_head FROM workspaces")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(head_before, 20);
    let reactions_before: i64 = sqlx::query_scalar("SELECT count(*) FROM message_reactions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(reactions_before > 0, "the old data has reactions to keep");

    // This release starts: migration 0010 applies on top and changes nothing that was there.
    db::migrate(&pool).await.expect("migration 0010");
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(applied, (1..=15).collect::<Vec<i64>>());
    let emoji: i64 = sqlx::query_scalar("SELECT count(*) FROM custom_emoji")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(emoji, 0);
    let reactions_after: i64 = sqlx::query_scalar("SELECT count(*) FROM message_reactions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(reactions_after, reactions_before);

    let alice = account("alice", "1", 'a');
    let bob = account("bob", "2", 'b');
    let carol = account("carol", "3", 'c');
    let workspace_id: WorkspaceId = id("a0");
    let general: ConversationId = id("c1");
    let with_a_file: MessageId = id("f9");
    let server = testing::spawn_test_server_with(
        pool.clone(),
        ServerOptions {
            open_registration: true,
            open_workspace_creation: true,
            password_cost: PasswordCost::FAST_FOR_TESTS,
            retention: RetentionPolicy::disabled(),
        },
    )
    .await;
    let app = App {
        server,
        pool: pool.clone(),
        http: reqwest::Client::new(),
        workspace_id,
        general,
    };

    // The old rows and the old log read as before, and there is no emoji yet.
    let boot = app.bootstrap_ok(&carol).await;
    assert!(boot.workspaces[0].custom_emoji.is_empty());
    let old = app.get_message(&bob, with_a_file).await;
    assert_eq!(old.attachments.len(), 1);
    assert_eq!(old.document.custom_emoji(), vec![]);
    let mut ws = app.connect_ws(&carol).await;
    ws.subscribe(workspace_id, EventSeq::ZERO).await;
    let (events, head) = ws.replay().await;
    assert_eq!((head.get(), events.len()), (20, 20));
    assert!(
        events
            .iter()
            .all(|e| !matches!(e.event, Event::EmojiChanged { .. }))
    );

    // Custom emoji on top of the old rows: the owner adds one, everyone sees it, a member cannot.
    let denied_for_member = app
        .upload_emoji_raw(
            &bob,
            workspace_id,
            lazy_domain::EmojiId::generate(),
            png_bytes(40, 40, 255),
            Some("party_time"),
        )
        .await;
    let (status, error) = api_error(denied_for_member).await;
    assert_error(status, &error, ErrorCode::Forbidden);
    let party = app
        .upload_emoji(&alice, workspace_id, "party_time", png_bytes(300, 200, 255))
        .await;
    let boot = app.bootstrap_ok(&carol).await;
    assert_eq!(boot.workspaces[0].custom_emoji, vec![party.clone()]);
    assert_eq!(
        app.emoji_raw(&carol, party.id, reqwest::Method::GET, None, &[])
            .await
            .status()
            .as_u16(),
        200
    );

    // It is used in a new message, in an edit of an old message (written in the older document
    // version), and in reactions on old and new messages.
    let document: lazy_domain::MessageDocument = serde_json::from_value(serde_json::json!({
        "version": 4,
        "blocks": [{"type": "paragraph", "inlines": [
            {"type": "text", "text": "with a file, "},
            {"type": "emoji", "id": party.id, "name": party.name},
        ]}],
    }))
    .unwrap();
    let new = app
        .send_doc_raw(&bob, general, document.clone())
        .await
        .json::<lazy_protocol::SendMessageResponse>()
        .await
        .unwrap()
        .message;
    assert_eq!(new.document.custom_emoji(), vec![party.id]);
    let edited = app
        .call(
            reqwest::Method::PATCH,
            &lazy_protocol::api::paths::message(with_a_file),
            &bob,
            Some(serde_json::json!({
                "operation_id": OperationId::generate(),
                "base_revision": old.revision,
                "document": document,
            })),
        )
        .await;
    assert!(edited.status().is_success(), "{}", edited.status());
    let custom = format!("custom:{}", party.id);
    app.react(&carol, with_a_file, &custom, true).await;
    app.react(&alice, new.id, &custom, true).await;
    let kept = app.get_message(&alice, with_a_file).await;
    assert_eq!(kept.attachments, old.attachments);
    assert_eq!(kept.document.custom_emoji(), vec![party.id]);

    // Renamed and deleted: what used it is unchanged, the old reactions are untouched, and the
    // cleanup job copes with a database that has both old and new rows.
    let renamed = app.rename_emoji_raw(&alice, party.id, "party_hat").await;
    assert!(renamed.status().is_success());
    assert_eq!(
        app.emoji_raw(&alice, party.id, reqwest::Method::DELETE, None, &[])
            .await
            .status()
            .as_u16(),
        204
    );
    let boot = app.bootstrap_ok(&carol).await;
    assert_eq!(boot.workspaces[0].custom_emoji.len(), 1);
    assert!(boot.workspaces[0].custom_emoji[0].deleted);
    assert_eq!(boot.workspaces[0].custom_emoji[0].name, "party_hat");
    assert_eq!(
        app.get_message(&bob, new.id).await.document.custom_emoji(),
        vec![party.id]
    );
    let store =
        lazy_server::attachments::store::FileStore::open_dir(app.server.attachments_dir()).unwrap();
    let report =
        lazy_server::attachments::cleanup(&pool, &store, std::time::Duration::from_secs(24 * 3600))
            .await
            .unwrap();
    assert_eq!(report.emoji_images, 1);
    assert_eq!(
        lazy_server::attachments::CleanupReport {
            emoji_images: 0,
            ..report
        },
        Default::default()
    );
    let swept = lazy_server::attachments::sweep_orphans(&pool, &store, std::time::Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(swept.orphans, 0);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn a_database_left_by_the_custom_emoji_release_upgrades_in_place_and_gains_search(
    pool: PgPool,
) {
    // Each release's migrations, and the data it left, in order, up to the custom emoji release.
    for (version, data) in [
        (5, POPULATED),
        (6, POPULATED_0006),
        (7, POPULATED_0007),
        (8, POPULATED_0008),
        (9, POPULATED_0009),
        (10, POPULATED_0010),
    ] {
        sqlx::migrate!("./migrations")
            .run_to(version, &pool)
            .await
            .unwrap_or_else(|e| panic!("migrations up to {version}: {e}"));
        sqlx::raw_sql(data)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("the data of {version}: {e}"));
    }
    let head_before: i64 = sqlx::query_scalar("SELECT event_head FROM workspaces")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(head_before, 21);

    // This release starts: migration 0011 applies on top. Existing rows are indexed by the
    // generated columns as the table is rewritten; nothing needs reindexing.
    db::migrate(&pool).await.expect("migration 0011");
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(applied, (1..=15).collect::<Vec<i64>>());
    let unindexed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM messages WHERE plain_text <> '' AND search_vector = ''::tsvector",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unindexed, 0);
    let unindexed_files: i64 =
        sqlx::query_scalar("SELECT count(*) FROM attachments WHERE name_vector = ''::tsvector")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(unindexed_files, 0);

    let alice = account("alice", "1", 'a');
    let bob = account("bob", "2", 'b');
    let carol = account("carol", "3", 'c');
    let workspace_id: WorkspaceId = id("a0");
    let general: ConversationId = id("c1");
    let server = testing::spawn_test_server_with(
        pool.clone(),
        ServerOptions {
            open_registration: true,
            open_workspace_creation: true,
            password_cost: PasswordCost::FAST_FOR_TESTS,
            retention: RetentionPolicy::disabled(),
        },
    )
    .await;
    let app = App {
        server,
        pool: pool.clone(),
        http: reqwest::Client::new(),
        workspace_id,
        general,
    };

    // The old messages are found as they were written: an edited one by its new text and not its
    // old, the deleted one not at all, a file by its name (the pending one not), and the message
    // with the custom emoji by the emoji's name, as text.
    let texts = |found: &lazy_protocol::SearchResponse| -> Vec<String> {
        found
            .hits
            .iter()
            .map(|hit| hit.message.document.plain_text())
            .collect()
    };
    assert_eq!(texts(&app.find(&bob, "edited").await), ["hello (edited)"]);
    assert_eq!(texts(&app.find(&bob, "plain").await), ["plain message"]);
    assert!(app.find(&bob, "deleted").await.hits.is_empty());
    let found = app.find(&bob, "notes").await;
    assert_eq!(found.hits.len(), 1);
    assert_eq!(found.hits[0].message.id, id::<MessageId>("f9"));
    assert!(matches!(
        found.hits[0].kind,
        lazy_protocol::SearchHitKind::Attachment { .. }
    ));
    assert!(app.find(&bob, "later").await.hits.is_empty());
    let found = app.find(&bob, "party").await;
    assert_eq!(texts(&found), ["launch day :party:"]);
    assert_eq!(found.hits[0].message.id, id::<MessageId>("fa"));
    // Filters over old rows: newest first, nothing deleted.
    let found = app.find(&bob, "in:general").await;
    assert_eq!(found.hits[0].message.id, id::<MessageId>("fa"));
    assert!(found.hits.iter().all(|hit| !hit.message.deleted));
    let carols = app.find(&bob, "from:carol").await;
    assert!(carols.hits.len() >= 2);
    assert_eq!(carols.hits[0].message.id, id::<MessageId>("fa"));
    // The old log replays unchanged.
    let mut ws = app.connect_ws(&carol).await;
    ws.subscribe(workspace_id, EventSeq::ZERO).await;
    let (events, head) = ws.replay().await;
    assert_eq!((head.get(), events.len()), (21, 21));

    // New activity on top: a new message is found, an edit reindexes, a deletion removes.
    let new = app.send(&alice, general, "searchable newcomer").await;
    assert_eq!(app.find(&bob, "newcomer").await.hits.len(), 1);
    let edited = app.edit(&alice, &new, "renamed newcomer").await;
    assert!(app.find(&bob, "searchable").await.hits.is_empty());
    assert_eq!(
        app.find(&bob, "renamed").await.hits[0].message.id,
        edited.id
    );
    let old = app.get_message(&bob, id("f1")).await;
    app.edit(&alice, &old, "hello again").await;
    assert!(app.find(&bob, "edited").await.hits.is_empty());
    assert_eq!(app.find(&bob, "again").await.hits.len(), 1);
    app.delete(&alice, new.id).await;
    assert!(app.find(&bob, "newcomer").await.hits.is_empty());
    app.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn a_database_left_by_the_search_release_upgrades_in_place_and_gains_attention(pool: PgPool) {
    // Each release's migrations, and the data it left, in order, up to the search release (which
    // added no data of its own: its columns are generated).
    for (version, data) in [
        (5, Some(POPULATED)),
        (6, Some(POPULATED_0006)),
        (7, Some(POPULATED_0007)),
        (8, Some(POPULATED_0008)),
        (9, Some(POPULATED_0009)),
        (10, Some(POPULATED_0010)),
        (11, None),
    ] {
        sqlx::migrate!("./migrations")
            .run_to(version, &pool)
            .await
            .unwrap_or_else(|e| panic!("migrations up to {version}: {e}"));
        if let Some(data) = data {
            sqlx::raw_sql(data)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("the data of {version}: {e}"));
        }
    }
    db::migrate(&pool).await.expect("migration 0012");
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(applied, (1..=15).collect::<Vec<i64>>());
    let rows: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM notification_settings) \
              + (SELECT count(*) FROM conversation_notification_prefs) \
              + (SELECT count(*) FROM user_status)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, 0);

    let alice = account("alice", "1", 'a');
    let bob = account("bob", "2", 'b');
    let carol = account("carol", "3", 'c');
    let workspace_id: WorkspaceId = id("a0");
    let general: ConversationId = id("c1");
    let thread_root: MessageId = id("f1");
    let server = testing::spawn_test_server_with(
        pool.clone(),
        ServerOptions {
            open_registration: true,
            open_workspace_creation: true,
            password_cost: PasswordCost::FAST_FOR_TESTS,
            retention: RetentionPolicy::disabled(),
        },
    )
    .await;
    let app = App {
        server,
        pool: pool.clone(),
        http: reqwest::Client::new(),
        workspace_id,
        general,
    };

    // Defaults everywhere, no status on anyone, and the old log is intact.
    let boot = app.bootstrap_ok(&bob).await;
    let workspace = &boot.workspaces[0];
    assert_eq!(
        workspace.notification_settings,
        lazy_domain::notification::NotificationSettings::default()
    );
    assert!(workspace.conversation_notification_prefs.is_empty());
    assert!(workspace.members.iter().all(|m| m.status.is_none()));
    assert_eq!(workspace.head.get(), 21);

    // The old thread state is an inbox: one thread, each person with their own unread count.
    let mine = app.inbox(&alice, &[]).await;
    assert_eq!(mine.entries.len(), 1);
    let entry = &mine.entries[0];
    assert_eq!(entry.root.id, thread_root);
    assert_eq!(entry.state.unread_count, 2);
    assert_eq!(
        entry.last_reply.as_ref().map(|m| m.id),
        Some(id::<MessageId>("f7"))
    );
    assert_eq!(entry.conversation.id, general);
    assert_eq!(app.inbox(&bob, &[]).await.entries[0].state.unread_count, 1);
    assert_eq!(
        app.inbox(&carol, &[]).await.entries[0].state.unread_count,
        0
    );
    assert!(
        app.inbox(&carol, &[("unread", "true")])
            .await
            .entries
            .is_empty()
    );

    // New features on top of the old rows.
    assert_eq!(app.inbox_read(&alice).await.updated, 1);
    assert!(
        app.inbox(&alice, &[("unread", "true")])
            .await
            .entries
            .is_empty()
    );
    let settings = lazy_domain::notification::NotificationSettings {
        desktop: false,
        ..Default::default()
    };
    app.set_settings(&alice, &settings).await;
    app.set_prefs(
        &alice,
        general,
        Some(lazy_domain::notification::NotifyLevel::Nothing),
        true,
    )
    .await;
    app.set_status(&alice, "Reviewing", Some("📝")).await;
    let boot = app.bootstrap_ok(&bob).await;
    let seen = boot.workspaces[0]
        .members
        .iter()
        .find(|m| m.id == alice.user.id)
        .unwrap();
    assert_eq!(seen.status.as_ref().map(|s| s.text()), Some("Reviewing"));
    let boot = app.bootstrap_ok(&alice).await;
    assert_eq!(boot.workspaces[0].notification_settings, settings);
    assert_eq!(boot.workspaces[0].conversation_notification_prefs.len(), 1);
    // The search release's index still serves the old rows.
    assert_eq!(app.find(&bob, "edited").await.hits.len(), 1);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn a_database_left_by_the_attention_release_upgrades_in_place_and_gains_discovery(
    pool: PgPool,
) {
    // Each release's migrations, and the data it left, in order, up to the attention release
    // (which, like search, added no data of its own).
    for (version, data) in [
        (5, Some(POPULATED)),
        (6, Some(POPULATED_0006)),
        (7, Some(POPULATED_0007)),
        (8, Some(POPULATED_0008)),
        (9, Some(POPULATED_0009)),
        (10, Some(POPULATED_0010)),
        (11, None),
        (12, None),
    ] {
        sqlx::migrate!("./migrations")
            .run_to(version, &pool)
            .await
            .unwrap_or_else(|e| panic!("migrations up to {version}: {e}"));
        if let Some(data) = data {
            sqlx::raw_sql(data)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("the data of {version}: {e}"));
        }
    }
    db::migrate(&pool).await.expect("migration 0013");
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(applied, (1..=15).collect::<Vec<i64>>());
    let saved_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM saved_messages")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(saved_rows, 0);

    let alice = account("alice", "1", 'a');
    let bob = account("bob", "2", 'b');
    let carol = account("carol", "3", 'c');
    let workspace_id: WorkspaceId = id("a0");
    let general: ConversationId = id("c1");
    let server = testing::spawn_test_server_with(
        pool.clone(),
        ServerOptions {
            open_registration: true,
            open_workspace_creation: true,
            password_cost: PasswordCost::FAST_FOR_TESTS,
            retention: RetentionPolicy::disabled(),
        },
    )
    .await;
    let app = App {
        server,
        pool: pool.clone(),
        http: reqwest::Client::new(),
        workspace_id,
        general,
    };

    // The old channels are the directory; the old people are the people directory (the bot of the
    // webhooks release is not listed but has a profile).
    let directory = app.browse(&bob, &[]).await;
    let listed: Vec<String> = directory
        .channels
        .iter()
        .map(|c| c.conversation.name.as_ref().unwrap().to_string())
        .collect();
    assert!(listed.contains(&"general".to_owned()) && listed.contains(&"random".to_owned()));
    assert!(
        directory
            .channels
            .iter()
            .all(|c| c.conversation.visibility == lazy_domain::ChannelVisibility::Public)
    );
    assert!(!directory.has_more);
    let people = app.people(&bob, &[]).await;
    let usernames: Vec<String> = people
        .people
        .iter()
        .map(|p| p.user.username.to_string())
        .collect();
    assert_eq!(usernames, ["alice", "bob", "carol"]);
    assert!(people.people.iter().all(|p| !p.user.is_bot));
    let bot: uuid::Uuid = sqlx::query_scalar("SELECT bot_user_id FROM webhooks LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let profile = app
        .person_raw(&bob, workspace_id, lazy_domain::UserId::from_uuid(bot))
        .await;
    assert!(profile.status().is_success());
    assert!(
        profile
            .json::<lazy_protocol::PersonResponse>()
            .await
            .unwrap()
            .person
            .user
            .is_bot
    );

    // Saved messages on old rows: an edited message, a reply in a thread, a message with a file;
    // the deleted one cannot be saved. The bootstrap and the list agree.
    let edited: MessageId = id("f1");
    let reply: MessageId = id("f6");
    let with_file: MessageId = id("f9");
    let deleted: MessageId = id("f2");
    for message in [edited, reply, with_file] {
        assert!(app.set_saved(&bob, message, true).await.saved);
    }
    let refused = app.set_saved_raw(&bob, deleted, true).await;
    assert_eq!(refused.status().as_u16(), 404);
    let page = app.saved_page(&bob, &[]).await;
    let order: Vec<MessageId> = page.items.iter().map(|i| i.message.id).collect();
    assert_eq!(order, [with_file, reply, edited]);
    assert_eq!(page.items[0].message.attachments.len(), 1);
    assert_eq!(page.items[1].message.thread_root, Some(edited));
    let boot = app.bootstrap_ok(&bob).await;
    assert_eq!(boot.workspaces[0].saved, order);
    assert!(!app.set_saved(&bob, edited, false).await.saved);
    assert_eq!(app.saved_page(&bob, &[]).await.items.len(), 2);
    // Nobody else's list changed.
    assert!(app.saved_page(&carol, &[]).await.items.is_empty());
    assert!(app.saved_page(&alice, &[]).await.items.is_empty());
    app.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn a_database_left_by_the_discovery_release_upgrades_in_place_and_gains_administration(
    pool: PgPool,
) {
    // Each release's migrations, and the data it left, in order, up to the discovery release (which,
    // like search and attention, added no data of its own).
    for (version, data) in [
        (5, Some(POPULATED)),
        (6, Some(POPULATED_0006)),
        (7, Some(POPULATED_0007)),
        (8, Some(POPULATED_0008)),
        (9, Some(POPULATED_0009)),
        (10, Some(POPULATED_0010)),
        (11, None),
        (12, None),
        (13, None),
    ] {
        sqlx::migrate!("./migrations")
            .run_to(version, &pool)
            .await
            .unwrap_or_else(|e| panic!("migrations up to {version}: {e}"));
        if let Some(data) = data {
            sqlx::raw_sql(data)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("the data of {version}: {e}"));
        }
    }
    db::migrate(&pool).await.expect("migration 0014");
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(applied, (1..=15).collect::<Vec<i64>>());
    let deactivated: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workspace_members WHERE deactivated_at IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let invitations: i64 = sqlx::query_scalar("SELECT count(*) FROM workspace_invitations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((deactivated, invitations), (0, 0));

    let alice = account("alice", "1", 'a');
    let bob = account("bob", "2", 'b');
    let carol = account("carol", "3", 'c');
    let workspace_id: WorkspaceId = id("a0");
    let general: ConversationId = id("c1");
    let server = testing::spawn_test_server_with(
        pool.clone(),
        ServerOptions {
            open_registration: true,
            open_workspace_creation: true,
            password_cost: PasswordCost::FAST_FOR_TESTS,
            retention: RetentionPolicy::disabled(),
        },
    )
    .await;
    let app = App {
        server,
        pool: pool.clone(),
        http: reqwest::Client::new(),
        workspace_id,
        general,
    };

    // The old workspace has the default settings, and nobody is deactivated.
    let boot = app.bootstrap_ok(&bob).await;
    let workspace = &boot.workspaces[0];
    assert_eq!(workspace.workspace.settings, Default::default());
    assert!(workspace.members.iter().all(|m| !m.deactivated));
    assert_eq!(workspace.role, lazy_domain::WorkspaceRole::Member);

    // Administration on the old rows: alice (the owner of the old data) promotes bob, bob makes an
    // invitation, someone registers with it, and the old workspace's settings change.
    assert!(
        app.set_role_raw(&alice, workspace_id, bob.user.id, "admin")
            .await
            .status()
            .is_success()
    );
    let invite = app
        .create_invitation(&bob, serde_json::json!({"max_uses": 1}))
        .await;
    let joined = app
        .try_register_with(
            "newcomer",
            "correct horse battery",
            Some(invite.code.expose()),
        )
        .await;
    assert!(joined.status().is_success());
    let renamed = app
        .patch_workspace_raw(&alice, workspace_id, serde_json::json!({"name": "Renamed"}))
        .await;
    assert!(renamed.status().is_success());
    assert_eq!(
        app.bootstrap_ok(&carol).await.workspaces[0].workspace.name,
        "Renamed"
    );

    // Deactivating someone with old threads, saves and reactions cuts everything and reactivating
    // restores it.
    assert_eq!(app.inbox(&carol, &[]).await.entries.len(), 1);
    app.deactivate(&alice, &carol, true).await;
    assert!(app.bootstrap_ok(&carol).await.workspaces.is_empty());
    assert_eq!(
        app.inbox_raw(&carol, workspace_id, &[])
            .await
            .status()
            .as_u16(),
        404
    );
    assert_eq!(
        app.history(&carol, general, &[]).await.status().as_u16(),
        404
    );
    // Their old message is still there for the others, by name.
    let page = app.history_ok(&bob, general, &[]).await;
    assert!(page.messages.iter().any(|m| m.author_id == carol.user.id));
    app.deactivate(&alice, &carol, false).await;
    assert_eq!(app.inbox(&carol, &[]).await.entries.len(), 1);
    assert!(
        !app.history_ok(&carol, general, &[])
            .await
            .messages
            .is_empty()
    );
    app.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn a_database_left_by_the_administration_release_upgrades_in_place_and_gains_profiles(
    pool: PgPool,
) {
    // Each release's migrations, and the data it left, in order, up to the administration release.
    for (version, data) in [
        (5, Some(POPULATED)),
        (6, Some(POPULATED_0006)),
        (7, Some(POPULATED_0007)),
        (8, Some(POPULATED_0008)),
        (9, Some(POPULATED_0009)),
        (10, Some(POPULATED_0010)),
        (11, None),
        (12, None),
        (13, None),
        (14, Some(POPULATED_0014)),
    ] {
        sqlx::migrate!("./migrations")
            .run_to(version, &pool)
            .await
            .unwrap_or_else(|e| panic!("migrations up to {version}: {e}"));
        if let Some(data) = data {
            sqlx::raw_sql(data)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("the data of {version}: {e}"));
        }
    }
    db::migrate(&pool).await.expect("migration 0015");
    db::migrate(&pool)
        .await
        .expect("a second run changes nothing");
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(applied, (1..=15).collect::<Vec<i64>>());
    // Nobody has a picture, and no workspace has a creator (the old one is the default one).
    let (pictured, avatars, creators): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM users WHERE avatar_version <> 0), \
                (SELECT count(*) FROM user_avatars), \
                (SELECT count(*) FROM workspaces WHERE created_by IS NOT NULL)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((pictured, avatars, creators), (0, 0, 0));

    let alice = account("alice", "1", 'a');
    let bob = account("bob", "2", 'b');
    let carol = account("carol", "3", 'c');
    let workspace_id: WorkspaceId = id("a0");
    let general: ConversationId = id("c1");
    let server = testing::spawn_test_server_with(
        pool.clone(),
        ServerOptions {
            open_registration: true,
            open_workspace_creation: true,
            password_cost: PasswordCost::FAST_FOR_TESTS,
            retention: RetentionPolicy::disabled(),
        },
    )
    .await;
    let app = App {
        server,
        pool: pool.clone(),
        http: reqwest::Client::new(),
        workspace_id,
        general,
    };

    // The old workspace reads as before (its settings, its deactivated person) with no pictures.
    let boot = app.bootstrap_ok(&bob).await;
    assert_eq!(boot.workspaces.len(), 1);
    let old = &boot.workspaces[0];
    assert_eq!(
        old.workspace.settings.channel_creation,
        lazy_domain::workspace::ChannelCreationPolicy::AdminsOnly
    );
    assert!(old.members.iter().all(|m| m.avatar_version == 0));
    assert!(
        old.members
            .iter()
            .any(|m| m.username.as_str() == "dora" && m.deactivated)
    );

    // Registration still lands in the old workspace; a person-made workspace is not where it goes.
    let newcomer = app.register("newcomer").await;
    let ids: Vec<_> = app
        .bootstrap_ok(&newcomer)
        .await
        .workspaces
        .iter()
        .map(|w| w.workspace.id)
        .collect();
    assert_eq!(ids, [workspace_id]);

    // Profiles on old rows: a new name and a picture reach the old workspace's log and its people.
    let head = app.head_of(workspace_id).await;
    app.rename_me(&carol, "Carol C.").await;
    let set = app.set_avatar(&carol, png_bytes(50, 50, 255)).await;
    assert_eq!(set.avatar_version, 1);
    let events = app.profile_events(workspace_id, head).await;
    let seen: Vec<(String, u32)> = events.iter().map(|e| (e.2.clone(), e.3)).collect();
    assert_eq!(
        seen,
        [("Carol C.".to_owned(), 0), ("Carol C.".to_owned(), 1)]
    );
    assert!(
        app.avatar_raw(&bob, carol.user.id, Some(1), &[])
            .await
            .status()
            .is_success()
    );
    // Dora, deactivated before, cannot see it and is not announced to.
    let dora: lazy_domain::UserId = id("d4");
    assert_eq!(
        app.avatar_raw(&bob, dora, Some(1), &[])
            .await
            .status()
            .as_u16(),
        404
    );

    // Making a workspace works beside the old one, and its people do not see the old one's.
    let made = app.create_workspace(&bob, "Bob's").await;
    assert_eq!(app.bootstrap_ok(&bob).await.workspaces.len(), 2);
    assert_eq!(
        app.bootstrap_ok(&alice).await.workspaces.len(),
        1,
        "alice is not in bob's workspace"
    );
    assert_eq!(made.members.len(), 1);

    // The old invitation (made before profiles) is accepted by someone from another workspace; the
    // revoked one is not; the deactivated person is refused and uses nothing.
    let invite = app
        .invitation_to(&bob, made.workspace.id, serde_json::json!({"max_uses": 1}))
        .await;
    let response = app
        .try_register_with("erin", PASSWORD, Some(invite.code.expose()))
        .await;
    assert!(response.status().is_success());
    let session: lazy_protocol::SessionResponse = response.json().await.unwrap();
    let erin = Account {
        token: session.token.expose().to_owned(),
        user: session.user,
    };
    let (status, error) = api_error(app.accept_raw(&erin, "upgrade-code-0014-revoked").await).await;
    assert_eq!(
        (status, error.code),
        (400, lazy_protocol::ErrorCode::InvitationInvalid)
    );
    let joined = app.accept(&erin, "upgrade-code-0014-abcdef").await;
    assert_eq!(joined.workspace.id, workspace_id);
    let uses: i32 = sqlx::query_scalar(
        "SELECT uses FROM workspace_invitations WHERE id = '01a00000-0000-7000-8000-000000000b01'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(uses, 2);
    app.server.shutdown().await.unwrap();
}
