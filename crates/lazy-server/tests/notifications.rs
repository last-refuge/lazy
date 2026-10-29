//! Notification settings, conversation preferences, status, and the thread inbox.

mod common;

use common::*;
use futures_util::future::join_all;
use lazy_domain::mention::MentionTarget;
use lazy_domain::notification::{
    ConversationNotificationPrefs, NotificationSettings, NotifyLevel, QuietHours,
};
use lazy_domain::{ConversationId, EventSeq, MessageId, Timestamp, WorkspaceId};
use lazy_protocol::{ErrorCode, Event, EventEnvelope, Message, PROTOCOL_VERSION};
use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;

struct World {
    app: App,
    alice: Account,
    bob: Account,
    carol: Account,
}

async fn world(pool: PgPool) -> World {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    app.set_role(&alice, "admin").await;
    let bob = app.register("bob").await;
    let carol = app.register("carol").await;
    World {
        app,
        alice,
        bob,
        carol,
    }
}

async fn finish(world: World) {
    world.app.server.shutdown().await.unwrap();
}

async fn denied(response: reqwest::Response, code: ErrorCode) -> lazy_protocol::ApiError {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
    error
}

fn ts(offset: time::Duration) -> Timestamp {
    Timestamp::from(OffsetDateTime::now_utc() + offset)
}

fn evening() -> QuietHours {
    QuietHours {
        start_minute: 1320,
        end_minute: 420,
        utc_offset_minutes: -420,
        days: QuietHours::EVERY_DAY,
    }
}

fn custom_settings() -> NotificationSettings {
    NotificationSettings {
        channel_level: NotifyLevel::All,
        direct_level: NotifyLevel::Mentions,
        thread_replies: false,
        channel_mentions: false,
        desktop: false,
        show_content: false,
        dnd_until: Some(ts(time::Duration::hours(2))),
        quiet_hours: Some(evening()),
    }
}

fn settings_json(settings: &NotificationSettings) -> serde_json::Value {
    serde_json::to_value(settings).unwrap()
}

/// The workspace's bootstrap entry for this person.
async fn workspace_of(
    app: &App,
    who: &Account,
    workspace: WorkspaceId,
) -> lazy_protocol::WorkspaceBootstrap {
    app.bootstrap_ok(who)
        .await
        .workspaces
        .into_iter()
        .find(|w| w.workspace.id == workspace)
        .expect("a member of the workspace")
}

/// A socket subscribed from the current head, with its replay drained.
async fn listener(app: &App, who: &Account) -> Ws {
    let mut ws = app.connect_ws(who).await;
    ws.subscribe(app.workspace_id, app.head().await).await;
    ws.replay().await;
    ws
}

/// What `ws` receives up to a marker: registering someone announces them to the whole workspace
/// after everything already committed.
async fn heard(app: &App, ws: &mut Ws) -> Vec<EventEnvelope> {
    let marker = app
        .register(&format!("marker{}", UserIdSuffix::next()))
        .await;
    ws.events_until_marker(marker.user.id).await
}

struct UserIdSuffix;
impl UserIdSuffix {
    fn next() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }
}

fn settings_events(events: &[EventEnvelope]) -> Vec<NotificationSettings> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::NotificationSettingsChanged { settings } => Some(*settings),
            _ => None,
        })
        .collect()
}

fn prefs_events(events: &[EventEnvelope]) -> Vec<ConversationNotificationPrefs> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::ConversationNotificationPrefsChanged { prefs } => Some(*prefs),
            _ => None,
        })
        .collect()
}

fn status_events(events: &[EventEnvelope]) -> Vec<(lazy_domain::UserId, Option<String>)> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::UserStatusChanged { user_id, status } => {
                Some((*user_id, status.as_ref().map(|s| s.text().to_owned())))
            }
            _ => None,
        })
        .collect()
}

// --- Settings ------------------------------------------------------------------------------

#[sqlx::test]
async fn settings_start_as_the_defaults_and_replace_whole_per_person_and_workspace(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let boot = workspace_of(app, &w.alice, app.workspace_id).await;
    assert_eq!(boot.notification_settings, NotificationSettings::default());
    assert!(boot.conversation_notification_prefs.is_empty());

    let custom = custom_settings();
    assert_eq!(app.set_settings(&w.alice, &custom).await, custom);
    assert_eq!(
        workspace_of(app, &w.alice, app.workspace_id)
            .await
            .notification_settings,
        custom
    );
    // Other people's settings, and other workspaces', are their own.
    assert_eq!(
        workspace_of(app, &w.bob, app.workspace_id)
            .await
            .notification_settings,
        NotificationSettings::default()
    );
    let elsewhere = app.other_workspace(&[(&w.alice, "member")]).await;
    assert_eq!(
        workspace_of(app, &w.alice, elsewhere)
            .await
            .notification_settings,
        NotificationSettings::default()
    );
    // The whole value is replaced: a smaller one does not keep the quiet hours.
    let smaller = NotificationSettings {
        desktop: false,
        ..NotificationSettings::default()
    };
    assert_eq!(app.set_settings(&w.alice, &smaller).await, smaller);
    let now = workspace_of(app, &w.alice, app.workspace_id)
        .await
        .notification_settings;
    assert_eq!(
        (now.quiet_hours, now.dnd_until, now.channel_level),
        (None, None, NotifyLevel::Mentions)
    );
    // Back to the defaults leaves no row.
    app.set_settings(&w.alice, &NotificationSettings::default())
        .await;
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM notification_settings")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    finish(w).await;
}

#[sqlx::test]
async fn bad_settings_are_invalid_requests_and_the_rest_is_stored_as_given(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let put = |body: serde_json::Value| {
        let (app, alice) = (&w.app, w.alice.clone());
        async move { app.set_settings_raw(&alice, app.workspace_id, body).await }
    };
    let base = settings_json(&NotificationSettings::default());
    let with = |key: &str, value: serde_json::Value| {
        let mut body = base.clone();
        body[key] = value;
        body
    };
    let hours = |start: u32, end: u32, offset: i32, days: u32| json!({"start_minute": start, "end_minute": end, "utc_offset_minutes": offset, "days": days});
    for bad in [
        hours(100, 100, 0, 127),
        hours(1440, 100, 0, 127),
        hours(100, 1440, 0, 127),
        hours(100, 200, 0, 0),
        hours(100, 200, 0, 128),
        hours(100, 200, 841, 127),
        hours(100, 200, -841, 127),
    ] {
        let error = denied(
            put(with("quiet_hours", bad.clone())).await,
            ErrorCode::InvalidRequest,
        )
        .await;
        assert!(
            error.message.contains("quiet hours"),
            "{bad}: {}",
            error.message
        );
    }
    // Fourteen hours either way is the edge of valid.
    assert!(
        put(with("quiet_hours", hours(60, 120, 840, 127)))
            .await
            .status()
            .is_success()
    );
    assert!(
        put(with("quiet_hours", hours(60, 120, -840, 1)))
            .await
            .status()
            .is_success()
    );

    // A snooze more than 366 days ahead is refused; just inside is fine.
    let far = serde_json::to_value(ts(time::Duration::days(367))).unwrap();
    denied(put(with("dnd_until", far)).await, ErrorCode::InvalidRequest).await;
    let near = serde_json::to_value(ts(time::Duration::days(365))).unwrap();
    assert!(put(with("dnd_until", near)).await.status().is_success());
    // One already over is accepted, stored as given, and means no snooze.
    let past = NotificationSettings {
        dnd_until: Some(ts(-time::Duration::days(3))),
        ..NotificationSettings::default()
    };
    assert_eq!(app.set_settings(&w.alice, &past).await, past);
    assert_eq!(
        workspace_of(app, &w.alice, app.workspace_id)
            .await
            .notification_settings,
        past
    );
    assert!(!past.is_paused(Timestamp::from(OffsetDateTime::now_utc())));

    // Not a settings value at all.
    for body in [
        with("channel_level", json!("loud")),
        with("desktop", json!("yes")),
        json!({"desktop": true}),
        json!("nothing"),
    ] {
        denied(put(body).await, ErrorCode::InvalidRequest).await;
    }
    // Not a member of the workspace, and not signed in.
    let outsider = app.register("outsider").await;
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
        .bind(outsider.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    denied(
        app.set_settings_raw(&outsider, app.workspace_id, base.clone())
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.set_settings_raw(&w.alice, WorkspaceId::generate(), base.clone())
            .await,
        ErrorCode::NotFound,
    )
    .await;
    let unsigned = app
        .http
        .put(app.url(&lazy_protocol::api::paths::notification_settings(
            app.workspace_id,
        )))
        .json(&base)
        .send()
        .await
        .unwrap();
    denied(unsigned, ErrorCode::Unauthenticated).await;
    finish(w).await;
}

#[sqlx::test]
async fn an_identical_request_commits_nothing_and_a_change_is_one_event(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let head = app.head().await;
    // The defaults over the defaults: nothing.
    app.set_settings(&w.alice, &NotificationSettings::default())
        .await;
    assert_eq!(app.head().await, head);
    let custom = custom_settings();
    app.set_settings(&w.alice, &custom).await;
    assert_eq!(app.head().await.get(), head.get() + 1);
    app.set_settings(&w.alice, &custom).await;
    app.set_settings(&w.alice, &custom).await;
    assert_eq!(
        app.head().await.get(),
        head.get() + 1,
        "a repeat appends nothing"
    );
    // A change of one field is one event again; the defaults are one more, and then nothing.
    let other = NotificationSettings {
        show_content: true,
        ..custom
    };
    app.set_settings(&w.alice, &other).await;
    app.set_settings(&w.alice, &NotificationSettings::default())
        .await;
    app.set_settings(&w.alice, &NotificationSettings::default())
        .await;
    assert_eq!(app.head().await.get(), head.get() + 3);
    finish(w).await;
}

#[sqlx::test]
async fn a_persons_own_events_reach_all_their_devices_and_nobody_else(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let general = app.general;
    let before = app.head().await;
    let mut phone = listener(app, &w.alice).await;
    let mut laptop = listener(app, &w.alice).await;
    let mut bob = listener(app, &w.bob).await;
    let custom = custom_settings();
    app.set_settings(&w.alice, &custom).await;
    let prefs = app
        .set_prefs(&w.alice, general, Some(NotifyLevel::Nothing), true)
        .await;
    // Someone else's change is not heard by Alice.
    app.set_settings(
        &w.bob,
        &NotificationSettings {
            desktop: false,
            ..NotificationSettings::default()
        },
    )
    .await;

    let marker = app.register("marker").await;
    let phone_events = phone.events_until_marker(marker.user.id).await;
    let laptop_events = laptop.events_until_marker(marker.user.id).await;
    let bob_events = bob.events_until_marker(marker.user.id).await;
    for events in [&phone_events, &laptop_events] {
        assert_eq!(settings_events(events), [custom]);
        assert_eq!(prefs_events(events), [prefs]);
    }
    assert_eq!(settings_events(&bob_events).len(), 1);
    assert_ne!(settings_events(&bob_events)[0], custom);
    assert!(prefs_events(&bob_events).is_empty());

    // A returning subscriber replays them from the old cursor, and only their own.
    let mut returning = app.connect_ws(&w.alice).await;
    returning.subscribe(app.workspace_id, before).await;
    let (replayed, _) = returning.replay().await;
    assert_eq!(settings_events(&replayed), [custom]);
    assert_eq!(prefs_events(&replayed), [prefs]);
    let mut other = app.connect_ws(&w.carol).await;
    other.subscribe(app.workspace_id, before).await;
    let (carols, _) = other.replay().await;
    assert!(settings_events(&carols).is_empty() && prefs_events(&carols).is_empty());
    finish(w).await;
}

#[sqlx::test]
async fn two_devices_setting_at_once_leave_the_last_write_and_both_events_in_order(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let mut ws = listener(app, &w.alice).await;
    for round in 0..10u16 {
        let make = |start: u16| NotificationSettings {
            quiet_hours: Some(QuietHours {
                start_minute: start,
                ..evening()
            }),
            ..NotificationSettings::default()
        };
        let (a, b) = (make(100 + round), make(300 + round));
        let (ra, rb) = tokio::join!(
            app.set_settings_raw(&w.alice, app.workspace_id, settings_json(&a)),
            app.set_settings_raw(&w.alice, app.workspace_id, settings_json(&b)),
        );
        assert!(ra.status().is_success() && rb.status().is_success());
        let events = heard(app, &mut ws).await;
        let got = settings_events(&events);
        assert_eq!(got.len(), 2, "round {round}");
        assert!(got.contains(&a) && got.contains(&b));
        // What is stored is what the last event says.
        let stored = workspace_of(app, &w.alice, app.workspace_id)
            .await
            .notification_settings;
        assert_eq!(
            stored, got[1],
            "round {round}: the last event is the last write"
        );
    }
    finish(w).await;
}

#[sqlx::test]
async fn the_same_request_from_several_devices_at_once_is_one_change(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let head = app.head().await;
    let custom = custom_settings();
    let responses = join_all(
        (0..6).map(|_| app.set_settings_raw(&w.alice, app.workspace_id, settings_json(&custom))),
    )
    .await;
    assert!(responses.iter().all(|r| r.status().is_success()));
    assert_eq!(app.head().await.get(), head.get() + 1, "one settings event");
    let body = json!({"level": "nothing", "muted": true});
    let responses =
        join_all((0..6).map(|_| app.set_prefs_raw(&w.alice, app.general, body.clone()))).await;
    assert!(responses.iter().all(|r| r.status().is_success()));
    assert_eq!(
        app.head().await.get(),
        head.get() + 2,
        "one preference event"
    );
    let status = json!({"status": {"text": "Busy"}});
    let responses =
        join_all((0..6).map(|_| app.set_status_raw(&w.alice, app.workspace_id, status.clone())))
            .await;
    assert!(responses.iter().all(|r| r.status().is_success()));
    assert_eq!(app.head().await.get(), head.get() + 3, "one status event");
    finish(w).await;
}

/// Settings, and a status, are compared with what is in force under a lock for the person and the
/// workspace; hold it the way another device's request would and show a request waiting for it.
#[sqlx::test]
async fn a_request_waits_for_the_persons_lock_for_that_workspace(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let head = app.head().await;
    for (key, what) in [("notify-settings", "settings"), ("status", "status")] {
        let mut holder = app.pool.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("{key}:{}:{}", w.alice.user.id, app.workspace_id))
            .execute(&mut *holder)
            .await
            .unwrap();
        let request = async {
            if what == "settings" {
                app.set_settings_raw(
                    &w.alice,
                    app.workspace_id,
                    settings_json(&custom_settings()),
                )
                .await
            } else {
                app.set_status_raw(
                    &w.alice,
                    app.workspace_id,
                    json!({"status": {"text": "Held"}}),
                )
                .await
            }
        };
        tokio::pin!(request);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(600), &mut request)
                .await
                .is_err(),
            "the {what} request did not wait for the lock"
        );
        holder.rollback().await.unwrap();
        let response = tokio::time::timeout(std::time::Duration::from_secs(20), request)
            .await
            .unwrap();
        assert!(response.status().is_success());
    }
    assert_eq!(app.head().await.get(), head.get() + 2);
    finish(w).await;
}

// --- Conversation preferences --------------------------------------------------------------

#[sqlx::test]
async fn a_preference_needs_a_conversation_the_caller_can_read_and_is_dropped_with_membership(
    pool: PgPool,
) {
    let w = world(pool).await;
    let app = &w.app;
    let (alice, bob, carol) = (&w.alice, &w.bob, &w.carol);
    let open = app.create_channel(alice, "open", "public").await;
    let secret = app.create_channel(alice, "secret", "private").await;
    assert!(
        app.add_members(alice, secret.id, &[bob])
            .await
            .status()
            .is_success()
    );
    let old = app.create_channel(alice, "old", "public").await;
    assert!(
        app.act(alice, old.id, "archive")
            .await
            .status()
            .is_success()
    );
    let dm = app.open(alice, &[bob]).await;

    // A member of a private conversation; someone merely reading a public channel; an archived one.
    assert_eq!(
        app.set_prefs(bob, secret.id, Some(NotifyLevel::Nothing), false)
            .await
            .level,
        Some(NotifyLevel::Nothing)
    );
    assert!(app.set_prefs(carol, open.id, None, true).await.muted);
    assert!(
        app.set_prefs(carol, old.id, Some(NotifyLevel::All), false)
            .await
            .level
            .is_some()
    );
    assert_eq!(
        app.set_prefs(bob, dm.id, None, true).await.conversation_id,
        dm.id
    );
    // A private conversation she is not in answers like one that does not exist.
    let hidden = denied(
        app.set_prefs_raw(carol, secret.id, json!({"level": "all", "muted": false}))
            .await,
        ErrorCode::NotFound,
    )
    .await;
    let missing = denied(
        app.set_prefs_raw(
            carol,
            ConversationId::generate(),
            json!({"level": "all", "muted": false}),
        )
        .await,
        ErrorCode::NotFound,
    )
    .await;
    assert_eq!(hidden.message, missing.message);
    denied(
        app.set_prefs_raw(carol, dm.id, json!({"level": "all", "muted": false}))
            .await,
        ErrorCode::NotFound,
    )
    .await;
    // A bad level, a missing field, and a person outside the workspace.
    for body in [
        json!({"level": "loud", "muted": false}),
        json!({"level": "all"}),
        json!({"muted": 1}),
    ] {
        denied(
            app.set_prefs_raw(bob, secret.id, body).await,
            ErrorCode::InvalidRequest,
        )
        .await;
    }
    let outsider = app.register("outsider").await;
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
        .bind(outsider.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    denied(
        app.set_prefs_raw(&outsider, open.id, json!({"level": "all", "muted": false}))
            .await,
        ErrorCode::NotFound,
    )
    .await;

    // The bootstrap carries the preferences that exist, for conversations she can read.
    sqlx::query("INSERT INTO conversation_notification_prefs (user_id, conversation_id, muted) VALUES ($1, $2, true)")
        .bind(carol.user.id.as_uuid())
        .bind(secret.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    let mut mine: Vec<ConversationId> = workspace_of(app, carol, app.workspace_id)
        .await
        .conversation_notification_prefs
        .iter()
        .map(|p| p.conversation_id)
        .collect();
    mine.sort();
    let mut expected = vec![open.id, old.id];
    expected.sort();
    assert_eq!(
        mine, expected,
        "the private conversation she is not in is left out"
    );
    assert_eq!(
        workspace_of(app, bob, app.workspace_id)
            .await
            .conversation_notification_prefs
            .len(),
        2
    );

    // A preference goes when the person leaves or is removed, and rejoining starts from defaults.
    assert!(
        app.remove_member(alice, secret.id, bob)
            .await
            .status()
            .is_success()
    );
    assert_eq!(pref_rows(app, bob, secret.id).await, 0);
    assert!(
        app.add_members(alice, secret.id, &[bob])
            .await
            .status()
            .is_success()
    );
    assert!(
        workspace_of(app, bob, app.workspace_id)
            .await
            .conversation_notification_prefs
            .iter()
            .all(|p| p.conversation_id != secret.id)
    );
    assert!(app.act(carol, open.id, "join").await.status().is_success());
    assert_eq!(pref_rows(app, carol, open.id).await, 1, "joining keeps it");
    assert!(app.act(carol, open.id, "leave").await.status().is_success());
    assert_eq!(
        pref_rows(app, carol, open.id).await,
        0,
        "leaving drops it, though she can still read it"
    );
    finish(w).await;
}

async fn pref_rows(app: &App, who: &Account, conversation: ConversationId) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM conversation_notification_prefs WHERE user_id = $1 AND conversation_id = $2",
    )
    .bind(who.user.id.as_uuid())
    .bind(conversation.as_uuid())
    .fetch_one(&app.pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn a_preference_with_nothing_to_say_is_no_row_and_a_repeat_is_no_event(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let general = app.general;
    let head = app.head().await;
    // The defaults first: no row, no event, and the answer is the default.
    let prefs = app.set_prefs(&w.bob, general, None, false).await;
    assert_eq!((prefs.level, prefs.muted), (None, false));
    assert_eq!(
        (pref_rows(app, &w.bob, general).await, app.head().await),
        (0, head)
    );
    // Each shape is a row; a repeat of it changes nothing.
    for (level, muted) in [
        (Some(NotifyLevel::All), false),
        (None, true),
        (Some(NotifyLevel::Mentions), true),
    ] {
        app.set_prefs(&w.bob, general, level, muted).await;
        let after = app.head().await;
        app.set_prefs(&w.bob, general, level, muted).await;
        assert_eq!(app.head().await, after);
        assert_eq!(pref_rows(app, &w.bob, general).await, 1);
    }
    // Reset: the row goes and one event says so; a second reset is nothing.
    let after = app.head().await;
    let reset = app.set_prefs(&w.bob, general, None, false).await;
    assert_eq!((reset.level, reset.muted), (None, false));
    assert_eq!(pref_rows(app, &w.bob, general).await, 0);
    assert_eq!(app.head().await.get(), after.get() + 1);
    app.set_prefs(&w.bob, general, None, false).await;
    assert_eq!(app.head().await.get(), after.get() + 1);
    finish(w).await;
}

#[sqlx::test]
async fn someone_only_reading_a_public_channel_still_hears_their_own_change(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let open = app.create_channel(&w.alice, "open", "public").await;
    let mut carol = listener(app, &w.carol).await;
    let mut alice = listener(app, &w.alice).await;
    let prefs = app.set_prefs(&w.carol, open.id, None, true).await;
    assert_eq!(prefs_events(&heard(app, &mut carol).await), [prefs]);
    assert!(prefs_events(&heard(app, &mut alice).await).is_empty());
    finish(w).await;
}

// --- Status ----------------------------------------------------------------------------------

#[sqlx::test]
async fn a_status_is_set_seen_by_the_workspace_announced_and_cleared(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let before = app.head().await;
    let mut bob = listener(app, &w.bob).await;
    let mut alice = listener(app, &w.alice).await;

    let user = app
        .set_status(&w.alice, "  In a meeting  ", Some("📅"))
        .await;
    assert_eq!(user.id, w.alice.user.id);
    let status = user.status.clone().expect("a status");
    assert_eq!(
        (status.text(), status.emoji.map(|e| e.to_string())),
        ("In a meeting", Some("📅".to_owned()))
    );
    // Others see it on the person in the bootstrap's members; the person's own entry too.
    for viewer in [&w.bob, &w.carol, &w.alice] {
        let members = workspace_of(app, viewer, app.workspace_id).await.members;
        let alice = members.iter().find(|m| m.id == w.alice.user.id).unwrap();
        assert_eq!(alice.status, user.status);
        assert!(
            members
                .iter()
                .filter(|m| m.id != w.alice.user.id)
                .all(|m| m.status.is_none())
        );
    }
    // The workspace hears it, the person's other devices too.
    let head = app.head().await;
    app.set_status(&w.alice, "In a meeting", Some("📅")).await;
    assert_eq!(
        app.head().await,
        head,
        "the same status again changes nothing"
    );
    app.set_status(&w.alice, "Back soon", None).await;
    let cleared = app.clear_status(&w.alice).await;
    assert_eq!(cleared.status, None);
    assert_eq!(app.head().await.get(), head.get() + 2);
    app.clear_status(&w.alice).await;
    assert_eq!(
        app.head().await.get(),
        head.get() + 2,
        "clearing nothing changes nothing"
    );
    let expected = vec![
        (w.alice.user.id, Some("In a meeting".to_owned())),
        (w.alice.user.id, Some("Back soon".to_owned())),
        (w.alice.user.id, None),
    ];
    assert_eq!(status_events(&heard(app, &mut bob).await), expected);
    assert_eq!(status_events(&heard(app, &mut alice).await), expected);
    // A returning subscriber replays them; so does someone who was not connected.
    let mut returning = app.connect_ws(&w.carol).await;
    returning.subscribe(app.workspace_id, before).await;
    assert_eq!(status_events(&returning.replay().await.0), expected);
    assert!(
        workspace_of(app, &w.bob, app.workspace_id)
            .await
            .members
            .iter()
            .all(|m| m.status.is_none())
    );

    // Someone who joins later is announced without one, and sees everyone else's.
    app.set_status(&w.bob, "Out", None).await;
    let newcomer = app.register("newcomer").await;
    let members = workspace_of(app, &newcomer, app.workspace_id).await.members;
    assert_eq!(
        members
            .iter()
            .find(|m| m.id == w.bob.user.id)
            .unwrap()
            .status
            .as_ref()
            .map(|s| s.text()),
        Some("Out")
    );
    assert_eq!(
        members
            .iter()
            .find(|m| m.id == newcomer.user.id)
            .unwrap()
            .status,
        None
    );
    finish(w).await;
}

#[sqlx::test]
async fn an_ended_status_disappears_without_an_announcement(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let until = ts(time::Duration::hours(1));
    let body = json!({"status": {"text": "Lunch", "expires_at": until}});
    assert!(
        app.set_status_raw(&w.alice, app.workspace_id, body.clone())
            .await
            .status()
            .is_success()
    );
    let seen = |who: &Account| {
        let (app, id) = (&w.app, w.alice.user.id);
        let who = who.clone();
        async move {
            workspace_of(app, &who, app.workspace_id)
                .await
                .members
                .into_iter()
                .find(|m| m.id == id)
                .unwrap()
                .status
        }
    };
    assert_eq!(
        seen(&w.bob).await.map(|s| s.text().to_owned()),
        Some("Lunch".to_owned())
    );
    // The same request again (to the microsecond the database keeps) is the same status.
    let head = app.head().await;
    let precise = json!({"status": {"text": "Lunch", "expires_at": until}});
    app.set_status_raw(&w.alice, app.workspace_id, precise)
        .await;
    assert_eq!(app.head().await, head);
    let nanos =
        json!({"status": {"text": "Precise", "expires_at": "2099-01-01T00:00:00.123456789Z"}});
    assert!(
        app.set_status_raw(&w.alice, app.workspace_id, nanos.clone())
            .await
            .status()
            .is_success()
    );
    let head = app.head().await;
    assert!(
        app.set_status_raw(&w.alice, app.workspace_id, nanos)
            .await
            .status()
            .is_success()
    );
    assert_eq!(
        app.head().await,
        head,
        "sub-microsecond digits do not make a repeat a change"
    );

    // It ends: nobody is told, and everybody stops seeing it.
    sqlx::query("UPDATE user_status SET expires_at = now() - interval '1 second'")
        .execute(&app.pool)
        .await
        .unwrap();
    let mut bob = listener(app, &w.bob).await;
    assert_eq!(seen(&w.bob).await, None);
    assert_eq!(seen(&w.alice).await, None);
    assert!(status_events(&heard(app, &mut bob).await).is_empty());
    // Clearing what nobody sees is nothing, and tidies the row; setting again works.
    let head = app.head().await;
    app.clear_status(&w.alice).await;
    assert_eq!(app.head().await, head);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM user_status")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    sqlx::query("INSERT INTO user_status (user_id, workspace_id, text, expires_at) VALUES ($1, $2, 'Old', now() - interval '1 day')")
        .bind(w.alice.user.id.as_uuid())
        .bind(app.workspace_id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    app.set_status(&w.alice, "Old", None).await;
    assert_eq!(
        app.head().await.get(),
        head.get() + 1,
        "an ended status is replaced, and that is announced"
    );

    // One that has already ended cannot be set.
    let past = json!({"status": {"text": "Late", "expires_at": ts(-time::Duration::seconds(5))}});
    denied(
        app.set_status_raw(&w.alice, app.workspace_id, past).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    finish(w).await;
}

#[sqlx::test]
async fn a_status_is_validated_by_its_own_type_and_its_emoji_must_be_usable(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let put = |status: serde_json::Value| {
        let (app, alice) = (&w.app, w.alice.clone());
        async move {
            app.set_status_raw(&alice, app.workspace_id, json!({"status": status}))
                .await
        }
    };
    let long = "x".repeat(101);
    for bad in [
        json!({"text": ""}),
        json!({"text": "   "}),
        json!({"text": long}),
        json!({"text": "bell\u{7}"}),
        json!({"text": "new\nline"}),
        json!({"text": "ok", "emoji": "not an emoji"}),
        json!({"text": "ok", "emoji": "custom:nonsense"}),
        json!({"emoji": "🎉"}),
        json!({"text": 5}),
    ] {
        let response = put(bad.clone()).await;
        assert_eq!(response.status().as_u16(), 400, "{bad}");
        denied(response, ErrorCode::InvalidRequest).await;
    }
    assert!(
        put(json!({"text": "x".repeat(100)}))
            .await
            .status()
            .is_success()
    );
    assert!(
        put(json!({"text": "party", "emoji": "🎉"}))
            .await
            .status()
            .is_success()
    );

    // A custom emoji: live in this workspace is fine; unknown, deleted, or another workspace's is not.
    let png = png_bytes(20, 20, 255);
    let live = app
        .upload_emoji(&w.alice, app.workspace_id, "fiesta", png.clone())
        .await;
    let gone = app
        .upload_emoji(&w.alice, app.workspace_id, "retired", png.clone())
        .await;
    app.emoji_raw(&w.alice, gone.id, reqwest::Method::DELETE, None, &[])
        .await;
    let elsewhere = app.other_workspace(&[(&w.alice, "admin")]).await;
    let foreign = app.upload_emoji(&w.alice, elsewhere, "abroad", png).await;
    let custom = |id: lazy_domain::EmojiId| json!({"text": "hi", "emoji": format!("custom:{id}")});
    assert!(put(custom(live.id)).await.status().is_success());
    for bad in [gone.id, foreign.id, lazy_domain::EmojiId::generate()] {
        denied(put(custom(bad)).await, ErrorCode::InvalidRequest).await;
    }
    let user = workspace_of(app, &w.bob, app.workspace_id).await.members;
    let shown = user
        .iter()
        .find(|m| m.id == w.alice.user.id)
        .unwrap()
        .status
        .clone()
        .unwrap();
    assert_eq!(
        shown.emoji.map(|e| e.to_string()),
        Some(format!("custom:{}", live.id))
    );

    // Outside the workspace: nothing is said, and nothing is shown to them.
    let outsider = app.register("outsider").await;
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
        .bind(outsider.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    denied(
        app.set_status_raw(&outsider, app.workspace_id, json!({"status": null}))
            .await,
        ErrorCode::NotFound,
    )
    .await;
    assert!(app.bootstrap_ok(&outsider).await.workspaces.is_empty());
    let unsigned = app
        .http
        .put(app.url(&lazy_protocol::api::paths::status(app.workspace_id)))
        .json(&json!({"status": null}))
        .send()
        .await
        .unwrap();
    denied(unsigned, ErrorCode::Unauthenticated).await;
    finish(w).await;
}

#[sqlx::test]
async fn a_bot_cannot_have_a_status(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let hook = app.create_webhook(&w.alice, app.general, "Deploybot").await;
    let status = lazy_domain::notification::UserStatus::new("Deploying", None, None).unwrap();
    let error = lazy_server::notifications::set_status(
        &app.pool,
        hook.webhook.bot_user_id,
        app.workspace_id,
        Some(status),
    )
    .await
    .unwrap_err();
    match error {
        lazy_server::AppError::Api(api) => assert_eq!(api.code, ErrorCode::Forbidden),
        other => panic!("{other:?}"),
    }
    finish(w).await;
}

// --- The inbox -------------------------------------------------------------------------------

/// A thread: a root by `author` and one reply by `replier` (so the author follows it).
async fn thread(
    app: &App,
    conversation: ConversationId,
    author: &Account,
    replier: &Account,
    tag: &str,
) -> (Message, Message) {
    let root = app.send(author, conversation, &format!("root {tag}")).await;
    let reply = app.reply(replier, &root, &format!("reply {tag}")).await;
    (root, reply)
}

fn last_seq(entry: &lazy_protocol::InboxEntry) -> EventSeq {
    entry
        .root
        .thread
        .as_ref()
        .expect("a summary")
        .last_reply_seq
}

#[sqlx::test]
async fn the_inbox_lists_followed_threads_newest_activity_first_with_everything_a_row_needs(
    pool: PgPool,
) {
    let w = world(pool).await;
    let app = &w.app;
    let (alice, bob) = (&w.alice, &w.bob);
    let eng = app.create_channel(alice, "eng", "public").await;
    let sec = app.create_channel(alice, "sec", "private").await;
    assert!(app.act(bob, eng.id, "join").await.status().is_success());
    assert!(
        app.add_members(alice, sec.id, &[bob])
            .await
            .status()
            .is_success()
    );
    let places = [app.general, eng.id, sec.id];
    let mut threads = Vec::new();
    for n in 0..8 {
        threads.push(thread(app, places[n % 3], alice, bob, &n.to_string()).await);
    }
    // More activity in an old thread moves it to the top.
    let bump = app.reply(bob, &threads[1].0, "bumped").await;
    let page = app.inbox(alice, &[]).await;
    assert_eq!(page.entries.len(), 8);
    assert!(!page.has_more);
    assert_eq!(page.entries[0].root.id, threads[1].0.id);
    assert_eq!(
        page.entries[0].last_reply.as_ref().map(|m| m.id),
        Some(bump.id)
    );
    let order: Vec<EventSeq> = page.entries.iter().map(last_seq).collect();
    assert!(
        order.windows(2).all(|w| w[0] > w[1]),
        "newest activity first"
    );
    // Each entry has what a row shows, as the other endpoints return it.
    for entry in &page.entries {
        assert_eq!(entry.root, app.get_message(alice, entry.root.id).await);
        let last = entry.last_reply.as_ref().unwrap();
        assert_eq!(*last, app.get_message(alice, last.id).await);
        assert_eq!(last.thread_root, Some(entry.root.id));
        assert!(entry.state.following);
        assert_eq!(
            Some(entry.state.clone()),
            app.thread_state(alice, entry.root.id).await
        );
        assert_eq!(
            entry.conversation,
            app.conversations_of(alice)
                .await
                .into_iter()
                .find(|c| c.id == entry.root.conversation_id)
                .unwrap()
        );
    }
    assert_eq!(page.entries[0].state.unread_count, 2);
    // Bob follows the same threads as a replier; Carol follows none.
    assert_eq!(app.inbox(bob, &[]).await.entries.len(), 8);
    assert!(app.inbox(&w.carol, &[]).await.entries.is_empty());

    // Paging by the position of the latest reply: no overlap, no gap, exact has_more.
    let mut seen = Vec::new();
    let mut before: Option<String> = None;
    loop {
        let mut query = vec![("limit", "3")];
        if let Some(b) = &before {
            query.push(("before", b.as_str()));
        }
        let page = app.inbox(alice, &query).await;
        assert!(page.entries.len() <= 3);
        seen.extend(page.entries.iter().map(|e| e.root.id));
        if !page.has_more {
            break;
        }
        assert_eq!(page.entries.len(), 3);
        before = Some(last_seq(page.entries.last().unwrap()).get().to_string());
    }
    let all: Vec<MessageId> = app
        .inbox(alice, &[])
        .await
        .entries
        .iter()
        .map(|e| e.root.id)
        .collect();
    assert_eq!(seen, all);
    // Sizes are clamped: zero is one, a thousand is the most (50).
    assert_eq!(app.inbox(alice, &[("limit", "0")]).await.entries.len(), 1);
    assert_eq!(
        app.inbox(alice, &[("limit", "1000")]).await.entries.len(),
        8
    );
    for bad in [("limit", "x"), ("before", "-1"), ("unread", "maybe")] {
        denied(
            app.inbox_raw(alice, app.workspace_id, &[bad]).await,
            ErrorCode::InvalidRequest,
        )
        .await;
    }
    finish(w).await;
}

#[sqlx::test]
async fn the_unread_filter_and_the_counts_follow_reading_deleting_and_mentions(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let (alice, bob) = (&w.alice, &w.bob);
    let (t0, _) = thread(app, app.general, alice, bob, "zero").await;
    let (t1, _) = thread(app, app.general, alice, bob, "one").await;
    let (t2, r2) = thread(app, app.general, alice, bob, "two").await;
    let unread = |page: &lazy_protocol::InboxPage| {
        page.entries.iter().map(|e| e.root.id).collect::<Vec<_>>()
    };
    assert_eq!(
        unread(&app.inbox(alice, &[("unread", "true")]).await),
        [t2.id, t1.id, t0.id]
    );

    // Reading one keeps it in the list but out of the unread view.
    let read = app
        .read_thread(alice, t1.id, EventSeq::new(1_000_000).unwrap())
        .await;
    assert_eq!(read.unread_count, 0);
    assert_eq!(
        unread(&app.inbox(alice, &[("unread", "true")]).await),
        [t2.id, t0.id]
    );
    let all = app.inbox(alice, &[]).await;
    assert_eq!(all.entries.len(), 3);
    assert_eq!(
        all.entries
            .iter()
            .find(|e| e.root.id == t1.id)
            .unwrap()
            .state
            .unread_count,
        0
    );
    // The cursor works in the unread view too.
    let first = app
        .inbox(alice, &[("unread", "true"), ("limit", "1")])
        .await;
    assert!(first.has_more);
    let cursor = last_seq(&first.entries[0]).get().to_string();
    let next = app
        .inbox(alice, &[("unread", "true"), ("before", &cursor)])
        .await;
    assert_eq!(unread(&next), [t0.id]);

    // A reply that names her counts as a mention; a second reply makes two unread.
    let named = app
        .reply_mentioning(
            bob,
            &t2,
            "ping @alice",
            &[("alice", MentionTarget::User { id: alice.user.id })],
            false,
        )
        .await;
    let entry = app
        .inbox(alice, &[])
        .await
        .entries
        .into_iter()
        .find(|e| e.root.id == t2.id)
        .unwrap();
    assert_eq!(
        (entry.state.unread_count, entry.state.mention_count),
        (2, 1)
    );
    assert_eq!(entry.last_reply.as_ref().map(|m| m.id), Some(named.id));
    // The newest reply is deleted: the previous one is the last, and one is unread.
    app.delete(bob, named.id).await;
    let entry = app
        .inbox(alice, &[])
        .await
        .entries
        .into_iter()
        .find(|e| e.root.id == t2.id)
        .unwrap();
    assert_eq!(
        (entry.state.unread_count, entry.state.mention_count),
        (1, 0)
    );
    assert_eq!(entry.last_reply.as_ref().map(|m| m.id), Some(r2.id));
    // Her own replies are not unread for her.
    app.reply(alice, &t2, "my own").await;
    let entry = app
        .inbox(alice, &[])
        .await
        .entries
        .into_iter()
        .find(|e| e.root.id == t2.id)
        .unwrap();
    assert_eq!(entry.state.unread_count, 0);
    finish(w).await;
}

#[sqlx::test]
async fn deleted_roots_unfollowed_threads_and_lost_access_leave_the_inbox(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let (alice, bob) = (&w.alice, &w.bob);
    let sec = app.create_channel(alice, "sec", "private").await;
    assert!(
        app.add_members(alice, sec.id, &[bob])
            .await
            .status()
            .is_success()
    );
    thread(app, app.general, alice, bob, "kept").await;
    let (tombstoned, reply) = thread(app, app.general, alice, bob, "tombstoned").await;
    let (emptied, emptied_reply) = thread(app, app.general, alice, bob, "emptied").await;
    let (unfollowed, _) = thread(app, app.general, alice, bob, "unfollowed").await;
    let (secret, _) = thread(app, sec.id, alice, bob, "secret").await;
    let ids =
        |page: lazy_protocol::InboxPage| page.entries.iter().map(|e| e.root.id).collect::<Vec<_>>();
    assert_eq!(ids(app.inbox(alice, &[]).await).len(), 5);

    // A deleted root that still has replies is a tombstone in the list; one with none left is gone.
    app.delete(alice, tombstoned.id).await;
    let page = app.inbox(alice, &[]).await;
    let entry = page
        .entries
        .iter()
        .find(|e| e.root.id == tombstoned.id)
        .expect("still listed");
    assert!(entry.root.deleted);
    assert_eq!(entry.root.thread.as_ref().map(|t| t.reply_count), Some(1));
    assert_eq!(entry.last_reply.as_ref().map(|m| m.id), Some(reply.id));
    app.delete(bob, emptied_reply.id).await;
    assert!(
        !ids(app.inbox(alice, &[]).await).contains(&emptied.id),
        "no replies left"
    );
    app.delete(alice, emptied.id).await;
    assert!(
        !ids(app.inbox(alice, &[]).await).contains(&emptied.id),
        "deleted with none left"
    );
    // Unfollowing removes it and following brings it back.
    app.follow(alice, unfollowed.id, false).await;
    assert!(!ids(app.inbox(alice, &[]).await).contains(&unfollowed.id));
    app.follow(alice, unfollowed.id, true).await;
    assert!(ids(app.inbox(alice, &[]).await).contains(&unfollowed.id));

    // Leaving a private conversation takes its threads out; the others still have them.
    assert!(
        app.inbox(alice, &[])
            .await
            .entries
            .iter()
            .any(|e| e.root.id == secret.id)
    );
    assert!(app.act(alice, sec.id, "leave").await.status().is_success());
    assert!(!ids(app.inbox(alice, &[]).await).contains(&secret.id));
    assert!(
        app.inbox(bob, &[])
            .await
            .entries
            .iter()
            .any(|e| e.root.id == secret.id)
    );
    // Not a member of the workspace: not found, whatever the id.
    let outsider = app.register("outsider").await;
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
        .bind(outsider.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    denied(
        app.inbox_raw(&outsider, app.workspace_id, &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.inbox_raw(alice, WorkspaceId::generate(), &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.inbox_read_raw(&outsider, app.workspace_id).await,
        ErrorCode::NotFound,
    )
    .await;
    let unsigned = app
        .http
        .get(app.url(&lazy_protocol::api::paths::inbox(app.workspace_id)))
        .send()
        .await
        .unwrap();
    denied(unsigned, ErrorCode::Unauthenticated).await;
    finish(w).await;
}

#[sqlx::test]
async fn marking_the_inbox_read_updates_each_followed_thread_and_tells_every_device(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let (alice, bob) = (&w.alice, &w.bob);
    let mut roots = Vec::new();
    for n in 0..12 {
        roots.push(thread(app, app.general, alice, bob, &n.to_string()).await.0);
    }
    // One she stopped following keeps its replies unread; one with only her own replies has none.
    app.follow(alice, roots[0].id, false).await;
    let own = app.send(alice, app.general, "own thread").await;
    app.reply(alice, &own, "alone").await;
    let mut device = listener(app, alice).await;
    let mut other = listener(app, bob).await;

    let done = app.inbox_read(alice).await;
    assert_eq!(done.updated, 11);
    let events = heard(app, &mut device).await;
    let states: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::ThreadStateChanged { state } => Some(state.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(states.len(), 11);
    assert!(
        states
            .iter()
            .all(|s| s.unread_count == 0 && s.mention_count == 0 && s.following)
    );
    assert!(states.iter().all(|s| s.root_id != roots[0].id));
    assert!(
        heard(app, &mut other)
            .await
            .iter()
            .all(|e| !matches!(e.event, Event::ThreadStateChanged { .. }))
    );
    for state in &states {
        assert_eq!(
            Some(state.clone()),
            app.thread_state(alice, state.root_id).await
        );
    }
    assert!(
        app.inbox(alice, &[("unread", "true")])
            .await
            .entries
            .is_empty()
    );
    assert_eq!(app.inbox(alice, &[]).await.entries.len(), 12 - 1 + 1);
    // Nothing more to do is nothing: no events.
    let head = app.head().await;
    assert_eq!(app.inbox_read(alice).await.updated, 0);
    assert_eq!(app.head().await, head);
    // A new reply makes one unread again, and only that one is marked next time.
    app.reply(bob, &roots[5], "again").await;
    assert_eq!(
        app.inbox(alice, &[("unread", "true")]).await.entries.len(),
        1
    );
    assert_eq!(app.inbox_read(alice).await.updated, 1);
    assert_eq!(
        app.thread_state(alice, roots[0].id)
            .await
            .map(|s| (s.following, s.unread_count)),
        Some((false, 1))
    );
    finish(w).await;
}

#[sqlx::test]
async fn marking_the_inbox_read_does_five_hundred_at_a_time(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    // 505 followed threads with one unread reply each, written directly (positions kept below the
    // workspace head so later events sort after them).
    sqlx::query("UPDATE workspaces SET event_head = 5000 WHERE id = $1")
        .bind(app.workspace_id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    sqlx::query(
        "WITH roots AS ( \
             INSERT INTO messages (id, conversation_id, author_id, document, plain_text, seq, created_at) \
             SELECT gen_random_uuid(), $1, $2, \
                    '{\"version\":2,\"blocks\":[{\"type\":\"paragraph\",\"inlines\":[{\"type\":\"text\",\"text\":\"r\"}]}]}', \
                    'r', n, now() FROM generate_series(1, 505) n RETURNING id, seq), \
         replies AS ( \
             INSERT INTO messages (id, conversation_id, author_id, document, plain_text, seq, created_at, thread_root_id, listed) \
             SELECT gen_random_uuid(), $1, $3, \
                    '{\"version\":2,\"blocks\":[{\"type\":\"paragraph\",\"inlines\":[{\"type\":\"text\",\"text\":\"x\"}]}]}', \
                    'x', 1000 + roots.seq, now(), roots.id, false FROM roots RETURNING thread_root_id, seq), \
         t AS ( \
             INSERT INTO threads (root_id, conversation_id, reply_count, last_reply_seq, last_reply_at, recent_repliers, as_of) \
             SELECT thread_root_id, $1, 1, seq, now(), ARRAY[$3::uuid], seq FROM replies) \
         INSERT INTO thread_user_state (user_id, root_id, conversation_id, following, last_read) \
         SELECT $2, id, $1, true, 0 FROM roots",
    )
    .bind(app.general.as_uuid())
    .bind(w.alice.user.id.as_uuid())
    .bind(w.bob.user.id.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();
    assert_eq!(
        app.inbox(&w.alice, &[("unread", "true"), ("limit", "50")])
            .await
            .entries
            .len(),
        50
    );
    let mut device = listener(app, &w.alice).await;
    assert_eq!(app.inbox_read(&w.alice).await.updated, 500);
    assert_eq!(app.inbox_read(&w.alice).await.updated, 5);
    assert_eq!(app.inbox_read(&w.alice).await.updated, 0);
    let heard = heard(app, &mut device).await;
    let count = heard
        .iter()
        .filter(|e| matches!(e.event, Event::ThreadStateChanged { .. }))
        .count();
    assert_eq!(count, 505);
    // Positions in the log are consecutive: one transaction's worth, then the next.
    let seqs: Vec<i64> = heard.iter().map(|e| e.seq.get()).collect();
    assert!(seqs.windows(2).all(|p| p[1] == p[0] + 1));
    finish(w).await;
}

#[sqlx::test]
async fn the_server_speaks_a_protocol_that_has_notifications(pool: PgPool) {
    const { assert!(PROTOCOL_VERSION >= 11) };
    let w = world(pool).await;
    let boot = w.app.bootstrap_ok(&w.alice).await;
    assert_eq!(boot.protocol_version, PROTOCOL_VERSION);
    finish(w).await;
}
