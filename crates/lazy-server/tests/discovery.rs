//! Discovery: the channel directory, the people directory and profiles, and saved messages.

mod common;

use common::*;
use futures_util::future::join_all;
use lazy_domain::{ConversationId, MessageId, UserId, WorkspaceId};
use lazy_protocol::{
    ErrorCode, Event, EventEnvelope, MAX_DIRECTORY_PAGE, PROTOCOL_VERSION, SAVED_BOOTSTRAP_LIMIT,
};
use serde_json::json;
use sqlx::PgPool;

struct World {
    app: App,
    alice: Account,
    bob: Account,
    carol: Account,
    dave: Account,
}

async fn world(pool: PgPool) -> World {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    app.set_role(&alice, "admin").await;
    let bob = app.register("bob").await;
    let carol = app.register("carol").await;
    let dave = app.register("dave").await;
    World {
        app,
        alice,
        bob,
        carol,
        dave,
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

async fn listener(app: &App, who: &Account) -> Ws {
    let mut ws = app.connect_ws(who).await;
    ws.subscribe(app.workspace_id, app.head().await).await;
    ws.replay().await;
    ws
}

fn saved_events(events: &[EventEnvelope]) -> Vec<(MessageId, bool)> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::SavedChanged { message_id, saved } => Some((*message_id, *saved)),
            _ => None,
        })
        .collect()
}

fn names(page: &lazy_protocol::BrowseChannelsResponse) -> Vec<String> {
    page.channels
        .iter()
        .map(|c| c.conversation.name.as_ref().unwrap().to_string())
        .collect()
}

async fn outsider(app: &App, name: &str) -> Account {
    let account = app.register(name).await;
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
        .bind(account.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    account
}

/// Inserts `count` public channels straight into the database, `chan001`... with `members` members
/// recorded in the count (which is all the directory reads).
async fn seed_channels(app: &App, count: i32) {
    sqlx::query(
        "INSERT INTO conversations (id, workspace_id, kind, name, visibility, member_count) \
         SELECT gen_random_uuid(), $1, 'channel', 'chan' || lpad(n::text, 3, '0'), 'public', n % 7 \
         FROM generate_series(1, $2) n",
    )
    .bind(app.workspace_id.as_uuid())
    .bind(count)
    .execute(&app.pool)
    .await
    .unwrap();
}

// --- The channel directory ---------------------------------------------------------------------

struct Directory {
    delta: lazy_protocol::Conversation,
    secret: lazy_protocol::Conversation,
}

async fn directory(w: &World) -> Directory {
    let app = &w.app;
    let alice = &w.alice;
    let make = |name: &'static str, visibility: &'static str| async move {
        app.create_channel(alice, name, visibility).await
    };
    let alpha = make("alpha", "public").await;
    let beta = make("beta", "public").await;
    let gamma = make("gamma-x", "public").await;
    let delta = make("delta", "public").await;
    let secret = make("secret", "private").await;
    for (conversation, body) in [
        (
            alpha.id,
            json!({"topic": "Release plans", "description": "We are 50% done"}),
        ),
        (beta.id, json!({"topic": "back\\slash"})),
        (gamma.id, json!({"topic": "snake_case here"})),
    ] {
        assert!(
            app.update(alice, conversation, body)
                .await
                .status()
                .is_success()
        );
    }
    // bob and carol join alpha; bob joins beta.
    for (who, conversation) in [(&w.bob, alpha.id), (&w.carol, alpha.id), (&w.bob, beta.id)] {
        assert!(
            app.act(who, conversation, "join")
                .await
                .status()
                .is_success()
        );
    }
    assert!(
        app.act(alice, delta.id, "archive")
            .await
            .status()
            .is_success()
    );
    // A direct conversation too.
    app.open(alice, &[&w.bob]).await;
    Directory { delta, secret }
}

#[sqlx::test]
async fn the_directory_lists_public_channels_by_name_and_never_private_ones(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let d = directory(&w).await;
    // Archived channels and private ones are left out by default.
    let page = app.browse(&w.bob, &[]).await;
    assert_eq!(names(&page), ["alpha", "beta", "gamma-x", "general"]);
    assert!(!page.has_more);
    // Whether the caller joined each.
    let joined: Vec<(String, bool)> = page
        .channels
        .iter()
        .map(|c| (c.conversation.name.as_ref().unwrap().to_string(), c.joined))
        .collect();
    assert_eq!(
        joined,
        [
            ("alpha".to_owned(), true),
            ("beta".to_owned(), true),
            ("gamma-x".to_owned(), false),
            ("general".to_owned(), true)
        ]
    );
    // A channel's row is the conversation as everyone sees it.
    assert_eq!(
        page.channels[0].conversation.topic.as_deref(),
        Some("Release plans")
    );
    // Archived ones when asked for (and they say so).
    let all = app.browse(&w.bob, &[("include_archived", "true")]).await;
    assert_eq!(
        names(&all),
        ["alpha", "beta", "delta", "gamma-x", "general"]
    );
    assert!(
        all.channels
            .iter()
            .find(|c| c.conversation.id == d.delta.id)
            .unwrap()
            .conversation
            .archived
    );
    // A private channel and a direct conversation are never listed, even to their members.
    for member in [&w.alice, &w.bob] {
        for query in [
            &[][..],
            &[("joined", "true")],
            &[("include_archived", "true"), ("query", "secret")],
        ] {
            let page = app.browse(member, query).await;
            assert!(
                page.channels
                    .iter()
                    .all(|c| c.conversation.id != d.secret.id)
            );
            assert!(page.channels.iter().all(|c| c.conversation.name.is_some()));
        }
    }
    // Another workspace's channels are its own.
    let elsewhere = app.other_workspace(&[(&w.alice, "member")]).await;
    sqlx::query("INSERT INTO conversations (id, workspace_id, kind, name, visibility) VALUES ($1, $2, 'channel', 'abroad', 'public')")
        .bind(ConversationId::generate().as_uuid())
        .bind(elsewhere.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    assert!(!names(&app.browse(&w.alice, &[]).await).contains(&"abroad".to_owned()));
    let there: lazy_protocol::BrowseChannelsResponse = app
        .browse_raw(&w.alice, elsewhere, &[])
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(names(&there), ["abroad"]);
    // Not a member of the workspace; not signed in.
    let stranger = outsider(app, "stranger").await;
    denied(
        app.browse_raw(&stranger, app.workspace_id, &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.browse_raw(&w.alice, WorkspaceId::generate(), &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    let unsigned = app
        .http
        .get(app.url(&lazy_protocol::api::paths::browse_channels(
            app.workspace_id,
        )))
        .send()
        .await
        .unwrap();
    denied(unsigned, ErrorCode::Unauthenticated).await;
    finish(w).await;
}

#[sqlx::test]
async fn the_directory_filters_by_text_literally_by_membership_and_sorts_by_name_or_size(
    pool: PgPool,
) {
    let w = world(pool).await;
    let app = &w.app;
    directory(&w).await;
    let q = |text: &'static str| {
        let (app, who) = (&w.app, w.bob.clone());
        async move {
            names(
                &app.browse(&who, &[("query", text), ("include_archived", "true")])
                    .await,
            )
        }
    };
    // Name, topic and description; case-insensitive; a substring.
    assert_eq!(q("ALP").await, ["alpha"]);
    assert_eq!(q("release").await, ["alpha"]);
    assert_eq!(q("DONE").await, ["alpha"]);
    assert_eq!(
        q("a").await,
        ["alpha", "beta", "delta", "gamma-x", "general"]
    );
    // `%`, `_` and `\` are ordinary characters.
    assert_eq!(q("50%").await, ["alpha"]);
    assert_eq!(q("%").await, ["alpha"]);
    assert_eq!(q("_").await, ["gamma-x"]);
    assert_eq!(q("\\").await, ["beta"]);
    assert_eq!(q("snake_c").await, ["gamma-x"]);
    assert!(q("snake%case").await.is_empty());
    assert!(q(".").await.is_empty(), "not a pattern");
    assert!(q("nothing like it").await.is_empty());
    // Empty or blank text is no filter; surrounding blanks are ignored.
    assert_eq!(
        names(&app.browse(&w.bob, &[("query", "   ")]).await).len(),
        4
    );
    assert_eq!(q("  alp  ").await, ["alpha"]);
    // A NUL, which text cannot hold, is not a server error.
    assert!(
        app.browse_raw(&w.bob, app.workspace_id, &[("query", "a\0b")])
            .await
            .status()
            .is_success()
    );
    // Joined, not joined, both.
    assert_eq!(
        names(&app.browse(&w.bob, &[("joined", "true")]).await),
        ["alpha", "beta", "general"]
    );
    assert_eq!(
        names(&app.browse(&w.bob, &[("joined", "false")]).await),
        ["gamma-x"]
    );
    assert_eq!(
        names(&app.browse(&w.carol, &[("joined", "true")]).await),
        ["alpha", "general"]
    );
    assert_eq!(
        names(
            &app.browse(&w.bob, &[("joined", "false"), ("query", "alp")])
                .await
        ),
        Vec::<String>::new()
    );
    // Sorted by size: most members first, ties by name.
    let by_size = app.browse(&w.bob, &[("sort", "members")]).await;
    assert_eq!(names(&by_size), ["general", "alpha", "beta", "gamma-x"]);
    let counts: Vec<u32> = by_size
        .channels
        .iter()
        .map(|c| c.conversation.member_count)
        .collect();
    assert_eq!(counts, [4, 3, 2, 1]);
    assert_eq!(
        names(&app.browse(&w.bob, &[("sort", "name")]).await),
        ["alpha", "beta", "gamma-x", "general"]
    );
    // Bad parameters.
    for bad in [
        ("sort", "newest"),
        ("joined", "maybe"),
        ("limit", "x"),
        ("offset", "-1"),
        ("include_archived", "2"),
    ] {
        denied(
            app.browse_raw(&w.bob, app.workspace_id, &[bad]).await,
            ErrorCode::InvalidRequest,
        )
        .await;
    }
    // The longest text is 100 characters.
    let hundred = "a".repeat(100);
    assert!(
        app.browse_raw(&w.bob, app.workspace_id, &[("query", &hundred)])
            .await
            .status()
            .is_success()
    );
    let over = "a".repeat(101);
    let error = denied(
        app.browse_raw(&w.bob, app.workspace_id, &[("query", &over)])
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    assert!(error.message.contains("100"), "{}", error.message);
    finish(w).await;
}

#[sqlx::test]
async fn the_directory_pages_without_repeating_or_skipping_and_clamps_sizes(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    seed_channels(app, 130).await;
    let total = 131; // and #general
    // Walk every page of each order; every channel appears exactly once and in order.
    for sort in ["name", "members"] {
        for size in [7usize, 50, 100] {
            let mut seen: Vec<(String, u32)> = Vec::new();
            let mut offset = 0;
            loop {
                let page = app
                    .browse(
                        &w.bob,
                        &[
                            ("sort", sort),
                            ("limit", &size.to_string()),
                            ("offset", &offset.to_string()),
                        ],
                    )
                    .await;
                assert!(page.channels.len() <= size);
                seen.extend(page.channels.iter().map(|c| {
                    (
                        c.conversation.name.as_ref().unwrap().to_string(),
                        c.conversation.member_count,
                    )
                }));
                offset += page.channels.len();
                if !page.has_more {
                    break;
                }
                assert_eq!(page.channels.len(), size, "a page that has more is full");
            }
            assert_eq!(seen.len(), total, "{sort} by {size}");
            let mut unique: Vec<&String> = seen.iter().map(|(n, _)| n).collect();
            unique.sort();
            unique.dedup();
            assert_eq!(unique.len(), total);
            let mut expected = seen.clone();
            if sort == "name" {
                expected.sort_by(|a, b| a.0.cmp(&b.0));
            } else {
                expected.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            }
            assert_eq!(seen, expected, "{sort} by {size}");
        }
    }
    // Sizes: zero is one, a thousand is the most; the default is 50.
    assert_eq!(
        app.browse(&w.bob, &[("limit", "0")]).await.channels.len(),
        1
    );
    assert_eq!(
        app.browse(&w.bob, &[("limit", "1000")])
            .await
            .channels
            .len(),
        MAX_DIRECTORY_PAGE as usize
    );
    assert_eq!(app.browse(&w.bob, &[]).await.channels.len(), 50);
    // Offsets: past the end is empty, 100,000 is the furthest, one more is refused.
    let past = app.browse(&w.bob, &[("offset", "500")]).await;
    assert!(past.channels.is_empty() && !past.has_more);
    let furthest = app.browse(&w.bob, &[("offset", "100000")]).await;
    assert!(furthest.channels.is_empty() && !furthest.has_more);
    denied(
        app.browse_raw(&w.bob, app.workspace_id, &[("offset", "100001")])
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    // Exactly full is not "more".
    let exact = app
        .browse(&w.bob, &[("limit", "100"), ("offset", "31")])
        .await;
    assert_eq!((exact.channels.len(), exact.has_more), (100, false));
    let almost = app
        .browse(&w.bob, &[("limit", "100"), ("offset", "30")])
        .await;
    assert_eq!((almost.channels.len(), almost.has_more), (100, true));
    finish(w).await;
}

// --- People ----------------------------------------------------------------------------------

async fn rename(app: &App, who: &Account, display: &str) {
    sqlx::query("UPDATE users SET display_name = $2 WHERE id = $1")
        .bind(who.user.id.as_uuid())
        .bind(display)
        .execute(&app.pool)
        .await
        .unwrap();
}

fn person_names(page: &lazy_protocol::PeoplePage) -> Vec<String> {
    page.people
        .iter()
        .map(|p| p.user.username.to_string())
        .collect()
}

#[sqlx::test]
async fn people_are_paged_by_display_name_then_username_and_bots_are_not_listed(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    // adam / Adam tie on the name, so the username decides; the order ignores case.
    rename(app, &w.alice, "Zed Alpha").await;
    rename(app, &w.bob, "adam").await;
    rename(app, &w.carol, "Adam").await;
    rename(app, &w.dave, "Brenda").await;
    let hook = app.create_webhook(&w.alice, app.general, "Deploybot").await;
    let page = app.people(&w.bob, &[]).await;
    assert_eq!(person_names(&page), ["bob", "carol", "dave", "alice"]);
    assert!(!page.has_more);
    assert!(page.people.iter().all(|p| !p.user.is_bot));
    assert!(!person_names(&page).iter().any(|n| n.contains("deploybot")));
    // Roles and join dates are what the workspace says.
    for person in &page.people {
        let (role, joined): (String, time::OffsetDateTime) = sqlx::query_as(
            "SELECT role, joined_at FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
        )
        .bind(app.workspace_id.as_uuid())
        .bind(person.user.id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
        assert_eq!(serde_json::to_value(person.role).unwrap(), json!(role));
        assert_eq!(person.joined_at.as_offset_date_time(), joined);
    }
    assert_eq!(
        serde_json::to_value(
            page.people
                .iter()
                .find(|p| p.user.username.as_str() == "alice")
                .unwrap()
                .role
        )
        .unwrap(),
        json!("admin")
    );
    // Paging.
    let first = app.people(&w.bob, &[("limit", "3")]).await;
    assert_eq!(
        (person_names(&first), first.has_more),
        (vec!["bob".into(), "carol".into(), "dave".into()], true)
    );
    let next = app.people(&w.bob, &[("limit", "3"), ("offset", "3")]).await;
    assert_eq!(
        (person_names(&next), next.has_more),
        (vec!["alice".into()], false)
    );
    assert_eq!(app.people(&w.bob, &[("limit", "0")]).await.people.len(), 1);
    // Exactly full is not "more".
    let exact = app.people(&w.bob, &[("limit", "4")]).await;
    assert_eq!((exact.people.len(), exact.has_more), (4, false));
    assert!(
        app.people(&w.bob, &[("offset", "50")])
            .await
            .people
            .is_empty()
    );
    denied(
        app.people_raw(&w.bob, app.workspace_id, &[("offset", "100001")])
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    // A bot has a profile, marked.
    let response = app
        .person_raw(&w.bob, app.workspace_id, hook.webhook.bot_user_id)
        .await;
    assert!(response.status().is_success());
    let profile: lazy_protocol::PersonResponse = response.json().await.unwrap();
    assert!(profile.person.user.is_bot);
    finish(w).await;
}

#[sqlx::test]
async fn a_people_search_matches_names_literally_and_a_profile_carries_the_status(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    rename(app, &w.alice, "Zed").await;
    rename(app, &w.bob, "Bob 50% Real").await;
    rename(app, &w.carol, "Carol_Under\\score").await;
    let find = |text: &'static str| {
        let (app, who) = (&w.app, w.dave.clone());
        async move { person_names(&app.people(&who, &[("query", text)]).await) }
    };
    assert_eq!(
        find("BOB").await,
        ["bob"],
        "display name or username, any case"
    );
    assert_eq!(find("alic").await, ["alice"], "the username alone matches");
    assert_eq!(find("real").await, ["bob"]);
    assert_eq!(find("%").await, ["bob"]);
    assert_eq!(find("_").await, ["carol"]);
    assert_eq!(find("\\").await, ["carol"]);
    assert_eq!(find("50% r").await, ["bob"]);
    assert!(find(".").await.is_empty());
    assert_eq!(
        person_names(&app.people(&w.dave, &[("query", "  ")]).await).len(),
        4
    );
    assert!(
        app.people_raw(&w.dave, app.workspace_id, &[("query", "a\0b")])
            .await
            .status()
            .is_success()
    );
    let over = "a".repeat(101);
    denied(
        app.people_raw(&w.dave, app.workspace_id, &[("query", &over)])
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;

    // A status shows on the person unless it has ended.
    app.set_status(&w.bob, "In a meeting", Some("📅")).await;
    let profile = |who: &Account, target: &Account| {
        let (app, who, id) = (&w.app, who.clone(), target.user.id);
        async move {
            let response = app.person_raw(&who, app.workspace_id, id).await;
            assert!(response.status().is_success());
            response
                .json::<lazy_protocol::PersonResponse>()
                .await
                .unwrap()
                .person
        }
    };
    let bob = profile(&w.dave, &w.bob).await;
    assert_eq!(
        bob.user.status.as_ref().map(|s| s.text()),
        Some("In a meeting")
    );
    assert_eq!(
        app.people(&w.dave, &[("query", "bob")]).await.people[0]
            .user
            .status,
        bob.user.status
    );
    sqlx::query("UPDATE user_status SET expires_at = now() - interval '1 second'")
        .execute(&app.pool)
        .await
        .unwrap();
    assert_eq!(profile(&w.dave, &w.bob).await.user.status, None);
    assert_eq!(
        app.people(&w.dave, &[("query", "bob")]).await.people[0]
            .user
            .status,
        None
    );
    // One's own profile; the fields match the list.
    let me = profile(&w.carol, &w.carol).await;
    assert_eq!(me.user.id, w.carol.user.id);
    assert_eq!(
        me,
        app.people(&w.carol, &[("query", "carol")]).await.people[0]
    );

    // Someone only in another workspace, an unknown id, a stranger asking.
    let elsewhere = app.other_workspace(&[(&w.alice, "member")]).await;
    let visitor = app.register("visitor").await;
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1 AND workspace_id = $2")
        .bind(visitor.user.id.as_uuid())
        .bind(app.workspace_id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, 'member')",
    )
    .bind(elsewhere.as_uuid())
    .bind(visitor.user.id.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();
    denied(
        app.person_raw(&w.dave, app.workspace_id, visitor.user.id)
            .await,
        ErrorCode::NotFound,
    )
    .await;
    assert!(
        app.person_raw(&w.alice, elsewhere, visitor.user.id)
            .await
            .status()
            .is_success()
    );
    denied(
        app.person_raw(&w.dave, app.workspace_id, UserId::generate())
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.person_raw(&visitor, app.workspace_id, w.bob.user.id)
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.people_raw(&visitor, app.workspace_id, &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    assert!(!person_names(&app.people(&w.dave, &[]).await).contains(&"visitor".to_owned()));
    denied(
        app.people_raw(&w.dave, WorkspaceId::generate(), &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    let unsigned = app
        .http
        .get(app.url(&format!("/v1/workspaces/{}/people", app.workspace_id)))
        .send()
        .await
        .unwrap();
    denied(unsigned, ErrorCode::Unauthenticated).await;
    finish(w).await;
}

#[sqlx::test]
async fn the_people_directory_walks_every_person_once_and_clamps_sizes(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    sqlx::query("INSERT INTO users (id, username, display_name, password_hash) SELECT gen_random_uuid(), 'p' || lpad(n::text, 4, '0'), 'Person ' || (n % 17), 'x' FROM generate_series(1, 130) n")
        .execute(&app.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_members (workspace_id, user_id, role) SELECT $1, id, 'member' FROM users WHERE username LIKE 'p%'")
        .bind(app.workspace_id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    let total = 134;
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut offset = 0;
    loop {
        let page = app
            .people(&w.bob, &[("limit", "40"), ("offset", &offset.to_string())])
            .await;
        seen.extend(page.people.iter().map(|p| {
            (
                p.user.display_name.to_string().to_lowercase(),
                p.user.username.to_string(),
            )
        }));
        offset += page.people.len();
        if !page.has_more {
            break;
        }
    }
    assert_eq!(seen.len(), total);
    let mut sorted = seen.clone();
    sorted.sort();
    assert_eq!(seen, sorted, "by display name, then username");
    sorted.dedup();
    assert_eq!(sorted.len(), total);
    assert_eq!(
        app.people(&w.bob, &[("limit", "1000")]).await.people.len(),
        MAX_DIRECTORY_PAGE as usize
    );
    assert_eq!(app.people(&w.bob, &[]).await.people.len(), 50);
    finish(w).await;
}

// --- Saved messages --------------------------------------------------------------------------

#[sqlx::test]
async fn saving_is_idempotent_announces_each_real_change_once_and_only_to_the_person(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let message = app.send(&w.alice, app.general, "worth keeping").await;
    let other = app.send(&w.alice, app.general, "also this").await;
    let before = app.head().await;
    let mut phone = listener(app, &w.bob).await;
    let mut laptop = listener(app, &w.bob).await;
    let mut alice = listener(app, &w.alice).await;

    let response = app.set_saved(&w.bob, message.id, true).await;
    assert_eq!((response.message_id, response.saved), (message.id, true));
    let head = app.head().await;
    assert_eq!(head.get(), before.get() + 1);
    // Again: the same answer, nothing else.
    assert!(app.set_saved(&w.bob, message.id, true).await.saved);
    assert_eq!(app.head().await, head);
    // Removing what is not saved is nothing.
    assert!(!app.set_saved(&w.bob, other.id, false).await.saved);
    assert_eq!(app.head().await, head);
    // Removing is one event, and a second removal none.
    app.set_saved(&w.bob, message.id, false).await;
    app.set_saved(&w.bob, message.id, false).await;
    assert_eq!(app.head().await.get(), head.get() + 1);
    app.set_saved(&w.bob, message.id, true).await;

    let marker = app.register("marker").await;
    let expected = vec![(message.id, true), (message.id, false), (message.id, true)];
    assert_eq!(
        saved_events(&phone.events_until_marker(marker.user.id).await),
        expected
    );
    assert_eq!(
        saved_events(&laptop.events_until_marker(marker.user.id).await),
        expected
    );
    assert!(saved_events(&alice.events_until_marker(marker.user.id).await).is_empty());
    // A returning subscriber replays them; nobody else's replay has any.
    let mut returning = app.connect_ws(&w.bob).await;
    returning.subscribe(app.workspace_id, before).await;
    assert_eq!(saved_events(&returning.replay().await.0), expected);
    let mut other_replay = app.connect_ws(&w.carol).await;
    other_replay.subscribe(app.workspace_id, before).await;
    assert!(saved_events(&other_replay.replay().await.0).is_empty());
    // Bad body, no sign-in.
    denied(
        app.put_json(
            &w.bob,
            &lazy_protocol::api::paths::message_saved(message.id),
            json!({"saved": "yes"}),
        )
        .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    let unsigned = app
        .http
        .put(app.url(&lazy_protocol::api::paths::message_saved(message.id)))
        .json(&json!({"saved": true}))
        .send()
        .await
        .unwrap();
    denied(unsigned, ErrorCode::Unauthenticated).await;
    finish(w).await;
}

#[sqlx::test]
async fn someone_only_reading_a_public_channel_hears_their_own_save_on_every_device(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let open = app.create_channel(&w.alice, "open", "public").await;
    let message = app.send(&w.alice, open.id, "for readers").await;
    let mut phone = listener(app, &w.carol).await;
    let mut laptop = listener(app, &w.carol).await;
    app.set_saved(&w.carol, message.id, true).await;
    let marker = app.register("marker").await;
    for ws in [&mut phone, &mut laptop] {
        assert_eq!(
            saved_events(&ws.events_until_marker(marker.user.id).await),
            [(message.id, true)]
        );
    }
    // And her saved list and bootstrap have it, though she is not in the channel.
    assert_eq!(app.saved_page(&w.carol, &[]).await.items.len(), 1);
    assert_eq!(
        app.bootstrap_ok(&w.carol).await.workspaces[0].saved,
        [message.id]
    );
    finish(w).await;
}

#[sqlx::test]
async fn the_list_is_newest_saved_first_resaving_keeps_the_place_and_unsave_then_save_moves_up(
    pool: PgPool,
) {
    let w = world(pool).await;
    let app = &w.app;
    let mut messages = Vec::new();
    for n in 0..7 {
        messages.push(
            app.send(&w.alice, app.general, &format!("message {n}"))
                .await,
        );
    }
    // Saved in a different order from sent.
    for n in [3, 0, 5, 1, 6, 2, 4] {
        app.set_saved(&w.bob, messages[n].id, true).await;
    }
    let ids = |page: &lazy_protocol::SavedPage| {
        page.items.iter().map(|i| i.message.id).collect::<Vec<_>>()
    };
    let page = app.saved_page(&w.bob, &[]).await;
    let order: Vec<MessageId> = [4, 2, 6, 1, 5, 0, 3]
        .iter()
        .map(|n| messages[*n].id)
        .collect();
    assert_eq!(ids(&page), order, "most recently saved first");
    assert!(!page.has_more);
    let cursors: Vec<u64> = page.items.iter().map(|i| i.cursor).collect();
    assert!(cursors.windows(2).all(|w| w[0] > w[1]), "{cursors:?}");
    // Each item is the message and the conversation as the other endpoints return them.
    for item in &page.items {
        assert_eq!(item.message, app.get_message(&w.bob, item.message.id).await);
        assert_eq!(item.conversation.id, app.general);
        assert!(item.saved_at.as_offset_date_time() <= time::OffsetDateTime::now_utc());
    }
    // Saving what is saved keeps its place; removing and saving again moves it to the top.
    app.set_saved(&w.bob, messages[3].id, true).await;
    assert_eq!(ids(&app.saved_page(&w.bob, &[]).await), order);
    app.set_saved(&w.bob, messages[3].id, false).await;
    assert_eq!(ids(&app.saved_page(&w.bob, &[]).await), order[..6]);
    app.set_saved(&w.bob, messages[3].id, true).await;
    let moved = app.saved_page(&w.bob, &[]).await;
    assert_eq!(moved.items[0].message.id, messages[3].id);
    assert!(moved.items[0].cursor > cursors[0]);
    // Paging by the last cursor: no overlap, no gap, exact has_more.
    let mut seen = Vec::new();
    let mut before: Option<String> = None;
    loop {
        let mut query = vec![("limit", "3")];
        if let Some(b) = &before {
            query.push(("before", b.as_str()));
        }
        let page = app.saved_page(&w.bob, &query).await;
        seen.extend(ids(&page));
        if !page.has_more {
            break;
        }
        assert_eq!(page.items.len(), 3);
        before = Some(page.items.last().unwrap().cursor.to_string());
    }
    assert_eq!(seen, ids(&moved));
    // Sizes are clamped; a huge cursor is "everything".
    assert_eq!(
        app.saved_page(&w.bob, &[("limit", "0")]).await.items.len(),
        1
    );
    assert_eq!(
        app.saved_page(&w.bob, &[("limit", "1000")])
            .await
            .items
            .len(),
        7
    );
    assert_eq!(
        app.saved_page(&w.bob, &[("before", "18446744073709551615")])
            .await
            .items
            .len(),
        7
    );
    assert!(
        app.saved_page(&w.bob, &[("before", "1")])
            .await
            .items
            .is_empty()
    );
    denied(
        app.saved_raw(&w.bob, app.workspace_id, &[("before", "-1")])
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    // Everyone's list is their own.
    assert!(app.saved_page(&w.carol, &[]).await.items.is_empty());
    // A reply is savable and carries its thread root.
    let reply = app.reply(&w.carol, &messages[0], "in a thread").await;
    app.set_saved(&w.bob, reply.id, true).await;
    let top = &app.saved_page(&w.bob, &[("limit", "1")]).await.items[0];
    assert_eq!(
        (top.message.id, top.message.thread_root),
        (reply.id, Some(messages[0].id))
    );
    finish(w).await;
}

#[sqlx::test]
async fn saving_needs_the_access_reading_has_and_lists_leave_out_what_is_not_readable_until_it_is(
    pool: PgPool,
) {
    let w = world(pool).await;
    let app = &w.app;
    let (alice, bob, carol, dave) = (&w.alice, &w.bob, &w.carol, &w.dave);
    let open = app.create_channel(alice, "open", "public").await;
    let secret = app.create_channel(alice, "secret", "private").await;
    assert!(
        app.add_members(alice, secret.id, &[bob])
            .await
            .status()
            .is_success()
    );
    let dm = app.open(alice, &[bob]).await;
    let m_general = app.send(alice, app.general, "needle general").await;
    let m_open = app.send(alice, open.id, "needle open").await;
    let m_secret = app.send(alice, secret.id, "needle secret").await;
    let m_dm = app.send(alice, dm.id, "needle dm").await;
    let stranger = outsider(app, "stranger").await;
    let everyone = [alice, bob, carol, dave, &stranger];
    let messages = [&m_general, &m_open, &m_secret, &m_dm];
    // Saving works exactly where reading the message works, and where search finds it.
    for who in everyone {
        let searchable: Vec<MessageId> = if std::ptr::eq(who, &stranger) {
            Vec::new()
        } else {
            app.find(who, "needle")
                .await
                .hits
                .iter()
                .map(|h| h.message.id)
                .collect()
        };
        for message in messages {
            let readable = app
                .get_message_raw(who, message.id)
                .await
                .status()
                .is_success();
            let saved = app.set_saved_raw(who, message.id, true).await;
            assert_eq!(
                saved.status().is_success(),
                readable,
                "{} / {}",
                who.user.username,
                message.document.plain_text()
            );
            assert_eq!(searchable.contains(&message.id), readable);
            if !readable {
                denied(saved, ErrorCode::NotFound).await;
            }
        }
    }
    // Who got what: carol reads the public channel she has not joined; dave likewise.
    let listed = |page: lazy_protocol::SavedPage| {
        let mut texts: Vec<String> = page
            .items
            .iter()
            .map(|i| i.message.document.plain_text())
            .collect();
        texts.sort();
        texts
    };
    assert_eq!(
        listed(app.saved_page(alice, &[]).await),
        [
            "needle dm",
            "needle general",
            "needle open",
            "needle secret"
        ]
    );
    assert_eq!(
        listed(app.saved_page(bob, &[]).await),
        [
            "needle dm",
            "needle general",
            "needle open",
            "needle secret"
        ]
    );
    assert_eq!(
        listed(app.saved_page(carol, &[]).await),
        ["needle general", "needle open"]
    );
    // A message nobody has: the message's own not_found; an unknown workspace.
    denied(
        app.set_saved_raw(bob, MessageId::generate(), true).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.saved_raw(bob, WorkspaceId::generate(), &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.saved_raw(&stranger, app.workspace_id, &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    // Another workspace's saved message is on its own list only.
    let elsewhere = app.other_workspace(&[(bob, "member")]).await;
    let foreign_conversation = ConversationId::generate();
    sqlx::query("INSERT INTO conversations (id, workspace_id, kind, name, visibility) VALUES ($1, $2, 'channel', 'abroad', 'public')")
        .bind(foreign_conversation.as_uuid())
        .bind(elsewhere.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    let foreign = app
        .seed_messages(foreign_conversation, bob, 1, |_| "abroad".to_owned())
        .await;
    sqlx::query("INSERT INTO saved_messages (user_id, message_id) VALUES ($1, $2)")
        .bind(bob.user.id.as_uuid())
        .bind(foreign[0].as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    assert_eq!(app.saved_page(bob, &[]).await.items.len(), 4);
    let there: lazy_protocol::SavedPage = app
        .saved_raw(bob, elsewhere, &[])
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(there.items.len(), 1);

    // Removed from the private channel: its message leaves the list and the bootstrap at once, can
    // no longer be saved or even removed, and comes back, in its place, when bob is let back in.
    let cursor_before = app
        .saved_page(bob, &[])
        .await
        .items
        .iter()
        .find(|i| i.message.id == m_secret.id)
        .unwrap()
        .cursor;
    assert!(
        app.remove_member(alice, secret.id, bob)
            .await
            .status()
            .is_success()
    );
    assert!(!listed(app.saved_page(bob, &[]).await).contains(&"needle secret".to_owned()));
    let boot = app.bootstrap_ok(bob).await;
    let main = boot
        .workspaces
        .iter()
        .find(|x| x.workspace.id == app.workspace_id)
        .unwrap();
    assert!(!main.saved.contains(&m_secret.id) && main.saved.contains(&m_general.id));
    denied(
        app.set_saved_raw(bob, m_secret.id, false).await,
        ErrorCode::NotFound,
    )
    .await;
    assert!(
        app.add_members(alice, secret.id, &[bob])
            .await
            .status()
            .is_success()
    );
    let back = app.saved_page(bob, &[]).await;
    assert_eq!(
        back.items
            .iter()
            .find(|i| i.message.id == m_secret.id)
            .map(|i| i.cursor),
        Some(cursor_before)
    );

    // A deleted message leaves the list, cannot be saved, but can be removed.
    app.delete(alice, m_general.id).await;
    assert!(!listed(app.saved_page(bob, &[]).await).contains(&"needle general".to_owned()));
    let boot = app.bootstrap_ok(bob).await;
    assert!(
        !boot
            .workspaces
            .iter()
            .find(|x| x.workspace.id == app.workspace_id)
            .unwrap()
            .saved
            .contains(&m_general.id)
    );
    denied(
        app.set_saved_raw(carol, m_general.id, true).await,
        ErrorCode::NotFound,
    )
    .await;
    let head = app.head().await;
    assert!(!app.set_saved(bob, m_general.id, false).await.saved);
    assert_eq!(
        app.head().await.get(),
        head.get() + 1,
        "removing a deleted message's save is a real change"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM saved_messages WHERE message_id = $1")
        .bind(m_general.id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(
        rows, 3,
        "the others' saves of it remain, as rows, out of every list"
    );
    finish(w).await;
}

#[sqlx::test]
async fn the_bootstrap_carries_the_newest_two_thousand_readable_saved_messages(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let count = SAVED_BOOTSTRAP_LIMIT + 25;
    let ids = app
        .seed_messages(app.general, &w.alice, count, |n| format!("saved {n}"))
        .await;
    // Saved in sequence order, so the last is the newest saved.
    sqlx::query(
        "INSERT INTO saved_messages (user_id, message_id) SELECT $1, id FROM messages ORDER BY seq",
    )
    .bind(w.bob.user.id.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();
    let first = app.bootstrap_ok(&w.bob).await.workspaces[0].saved.clone();
    assert_eq!(first.len(), SAVED_BOOTSTRAP_LIMIT);
    let expected: Vec<MessageId> = ids
        .iter()
        .rev()
        .take(SAVED_BOOTSTRAP_LIMIT)
        .copied()
        .collect();
    assert_eq!(first, expected, "newest saved first");
    // Deleted and unreadable ones do not take a place: the cap is filled from older ones.
    sqlx::query("UPDATE messages SET deleted = true, listed = false WHERE id = $1")
        .bind(ids[count - 1].as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    let secret = app.create_channel(&w.alice, "secret", "private").await;
    let hidden = app
        .seed_messages(secret.id, &w.alice, 3, |n| format!("hidden {n}"))
        .await;
    sqlx::query("INSERT INTO saved_messages (user_id, message_id) SELECT $1, unnest($2::uuid[])")
        .bind(w.bob.user.id.as_uuid())
        .bind(hidden.iter().map(|id| id.as_uuid()).collect::<Vec<_>>())
        .execute(&app.pool)
        .await
        .unwrap();
    let second = app.bootstrap_ok(&w.bob).await.workspaces[0].saved.clone();
    assert_eq!(second.len(), SAVED_BOOTSTRAP_LIMIT);
    let expected: Vec<MessageId> = ids[..count - 1]
        .iter()
        .rev()
        .take(SAVED_BOOTSTRAP_LIMIT)
        .copied()
        .collect();
    assert_eq!(second, expected);
    assert!(hidden.iter().all(|id| !second.contains(id)));
    // The list pages to the ones the bootstrap cut off.
    let mut seen = 0;
    let mut before: Option<String> = None;
    loop {
        let mut query = vec![("limit", "100")];
        if let Some(b) = &before {
            query.push(("before", b.as_str()));
        }
        let page = app.saved_page(&w.bob, &query).await;
        seen += page.items.len();
        if !page.has_more {
            break;
        }
        before = Some(page.items.last().unwrap().cursor.to_string());
    }
    assert_eq!(seen, count - 1);
    // A page is never more than a hundred, whatever is asked.
    assert_eq!(
        app.saved_page(&w.bob, &[("limit", "1000")])
            .await
            .items
            .len(),
        MAX_DIRECTORY_PAGE as usize
    );
    assert_eq!(app.saved_page(&w.bob, &[]).await.items.len(), 50);
    finish(w).await;
}

#[sqlx::test]
async fn two_devices_changing_the_same_message_at_once_leave_one_state_and_events_that_say_so(
    pool: PgPool,
) {
    let w = world(pool).await;
    let app = &w.app;
    let message = app.send(&w.alice, app.general, "contested").await;
    let mut ws = listener(app, &w.bob).await;
    // Six saves at once are one change.
    let head = app.head().await;
    let responses = join_all((0..6).map(|_| app.set_saved_raw(&w.bob, message.id, true))).await;
    assert!(responses.iter().all(|r| r.status().is_success()));
    assert_eq!(app.head().await.get(), head.get() + 1);
    // Saves and removals at once: whatever order they ran in, what is stored is what the last event
    // says, and the events alternate (each is a real change).
    for round in 0..8 {
        let flips =
            join_all((0..6).map(|n| app.set_saved_raw(&w.bob, message.id, (n + round) % 2 == 0)))
                .await;
        assert!(flips.iter().all(|r| r.status().is_success()));
    }
    let marker = app.register("marker").await;
    let events = saved_events(&ws.events_until_marker(marker.user.id).await);
    assert!(!events.is_empty() && events[0] == (message.id, true));
    assert!(
        events.windows(2).all(|p| p[0].1 != p[1].1),
        "each event is a change: {events:?}"
    );
    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM saved_messages WHERE user_id = $1 AND message_id = $2",
    )
    .bind(w.bob.user.id.as_uuid())
    .bind(message.id.as_uuid())
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(stored == 1, events.last().unwrap().1);
    assert_eq!(app.saved_page(&w.bob, &[]).await.items.len() as i64, stored);
    finish(w).await;
}

#[sqlx::test]
async fn the_server_speaks_a_protocol_that_has_discovery(pool: PgPool) {
    const { assert!(PROTOCOL_VERSION >= 12) };
    let w = world(pool).await;
    assert_eq!(
        w.app.bootstrap_ok(&w.alice).await.protocol_version,
        PROTOCOL_VERSION
    );
    assert!(
        w.app.bootstrap_ok(&w.alice).await.workspaces[0]
            .saved
            .is_empty()
    );
    finish(w).await;
}
