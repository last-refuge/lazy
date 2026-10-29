//! Search: who can find what, the query language, ranking and paging, snippets in characters, and
//! the limits that keep a search cheap.

mod common;

use std::time::Duration;

use common::*;
use futures_util::future::join_all;
use lazy_domain::mention::MentionTarget;
use lazy_domain::search::{MAX_FILTER_VALUES, MAX_QUERY_CHARS, MAX_QUERY_TERMS, SearchQuery};
use lazy_domain::{AttachmentId, ConversationId, MessageId, WorkspaceId};
use lazy_protocol::{
    ErrorCode, Message, PROTOCOL_VERSION, SearchHit, SearchHitKind, SearchResponse,
};
use lazy_server::SearchOptions;
use sqlx::PgPool;

struct World {
    app: App,
    alice: Account,
    bob: Account,
    carol: Account,
    dave: Account,
}

async fn world_with(app: App) -> World {
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

async fn world(pool: PgPool) -> World {
    world_with(App::start(pool).await).await
}

async fn finish(world: World) {
    world.app.server.shutdown().await.unwrap();
}

fn texts(found: &SearchResponse) -> Vec<String> {
    found
        .hits
        .iter()
        .map(|hit| hit.message.document.plain_text())
        .collect()
}

fn sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values
}

fn ids(found: &SearchResponse) -> Vec<MessageId> {
    found.hits.iter().map(|hit| hit.message.id).collect()
}

/// The pieces of a snippet's text that its ranges mark, counted in characters.
fn marked(hit: &SearchHit) -> Vec<String> {
    let chars: Vec<char> = hit.snippet.text.chars().collect();
    hit.snippet
        .highlights
        .iter()
        .map(|range| {
            assert!(
                range.start < range.end && range.end as usize <= chars.len(),
                "range {range:?} outside {:?}",
                hit.snippet.text
            );
            chars[range.start as usize..range.end as usize]
                .iter()
                .collect()
        })
        .collect()
}

async fn denied(response: reqwest::Response, code: ErrorCode) -> lazy_protocol::ApiError {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
    error
}

fn name_of(hit: &SearchHit) -> Option<String> {
    hit.conversation.name.as_ref().map(|name| name.to_string())
}

// --- The query language ------------------------------------------------------------------------

#[sqlx::test]
async fn words_match_as_prefixes_in_any_case_and_all_of_them_must(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    for text in [
        "Deploy failed on staging",
        "deployment finished",
        "unrelated chatter",
        "release notes for deploys",
    ] {
        app.send(&w.alice, app.general, text).await;
    }
    let found = app.find(&w.bob, "deplo").await;
    assert_eq!(
        sorted(texts(&found)),
        [
            "Deploy failed on staging",
            "deployment finished",
            "release notes for deploys"
        ]
    );
    assert_eq!(app.find(&w.bob, "DEPLOY").await.hits.len(), 3);
    let found = app.find(&w.bob, "deploy staging").await;
    assert_eq!(texts(&found), ["Deploy failed on staging"]);
    let hit = &found.hits[0];
    assert_eq!(hit.kind, SearchHitKind::Message);
    assert_eq!(hit.message.author_id, w.alice.user.id);
    assert_eq!(hit.conversation.id, app.general);
    assert_eq!(marked(hit), ["Deploy", "staging"]);
    // A prefix of a longer word marks the whole word.
    let found = app.find(&w.bob, "deplo finished").await;
    assert_eq!(marked(&found.hits[0]), ["deployment", "finished"]);
    // Nothing found is an empty page, not an error.
    let none = app.find(&w.bob, "zzzz").await;
    assert_eq!(
        (none.hits.len(), none.has_more, none.truncated),
        (0, false, false)
    );
    finish(w).await;
}

#[sqlx::test]
async fn phrases_need_adjacent_words_exclusions_remove_and_typing_is_forgiven(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    for text in [
        "the rollback plan is ready",
        "plan for rollback",
        "rollback plan draft",
    ] {
        app.send(&w.alice, app.general, text).await;
    }
    let both = |found: SearchResponse| sorted(texts(&found));
    assert_eq!(
        both(app.find(&w.bob, "\"rollback plan\"").await),
        ["rollback plan draft", "the rollback plan is ready"]
    );
    assert_eq!(app.find(&w.bob, "rollback plan").await.hits.len(), 3);
    assert_eq!(
        texts(&app.find(&w.bob, "\"rollback plan\" -draft").await),
        ["the rollback plan is ready"]
    );
    assert_eq!(
        both(app.find(&w.bob, "rollback -draft").await),
        ["plan for rollback", "the rollback plan is ready"]
    );
    assert!(app.find(&w.bob, "plan -rollback").await.hits.is_empty());
    // Exclusions are prefixes too, like the words they exclude.
    assert_eq!(app.find(&w.bob, "rollback -dra").await.hits.len(), 2);
    // An unclosed quote, as typed live, and a phrase's last word is a prefix.
    assert_eq!(app.find(&w.bob, "\"rollback pl").await.hits.len(), 2);
    let found = app.find(&w.bob, "\"rollback plan\"").await;
    assert!(found.hits.iter().all(|h| !marked(h).is_empty()));
    finish(w).await;
}

#[sqlx::test]
async fn nothing_typed_is_an_operator_and_nothing_hostile_is_a_server_error(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    for text in [
        "apple pie",
        "banana split",
        "x&y|z!(w)<->v:* literal",
        "it's a\\b",
    ] {
        app.send(&w.alice, app.general, text).await;
    }
    // `|` is not OR, `&` not AND, `!` not NOT, and `:*` not a prefix marker of the person's own.
    assert!(app.find(&w.bob, "apple | banana").await.hits.is_empty());
    assert_eq!(texts(&app.find(&w.bob, "apple & pie").await), ["apple pie"]);
    assert_eq!(texts(&app.find(&w.bob, "!apple").await), ["apple pie"]);
    assert_eq!(texts(&app.find(&w.bob, "app:*").await), ["apple pie"]);
    // The punctuation itself is findable, since the index and the query tokenise alike.
    assert_eq!(
        texts(&app.find(&w.bob, "x&y|z!(w)<->v:*").await),
        ["x&y|z!(w)<->v:* literal"]
    );
    assert_eq!(texts(&app.find(&w.bob, "it's").await), ["it's a\\b"]);
    assert_eq!(texts(&app.find(&w.bob, "a\\b").await), ["it's a\\b"]);

    let long = "a".repeat(MAX_QUERY_CHARS);
    let hostile = [
        "'",
        "''",
        "'''",
        "\"",
        "\"\"\"",
        "\\",
        "\\\\",
        "\\'",
        "'\\",
        "&",
        "|",
        "!",
        "!!",
        "(",
        ")",
        "()",
        "(a|b)&!c",
        "<",
        ">",
        "<->",
        "a<->b",
        "a<2>b",
        ":",
        "::",
        ":*",
        "*",
        "a:*",
        "a:*b",
        "'a':*",
        "\u{0}",
        "a\u{0}b",
        "\u{202e}x",
        "\u{1}\u{2}",
        "\u{ff}",
        "\u{10ffff}",
        "日本語",
        "e\u{301}",
        "🎉",
        "'; DROP TABLE messages; --",
        "\" OR 1=1 --",
        "%",
        "_",
        "*:*",
        "from:'",
        "in:\\",
        "after:'",
        "has:'",
        "-'",
        "-\\",
        "-\"",
        &long,
    ];
    for q in hostile {
        let response = app.search_raw(&w.bob, app.workspace_id, &[("q", q)]).await;
        let status = response.status().as_u16();
        assert!(status == 200 || status == 400, "{q:?} answered {status}");
        if status == 400 {
            let (_, error) = api_error(response).await;
            assert_eq!(error.code, ErrorCode::InvalidRequest, "{q:?}");
        }
    }
    // Nothing was damaged by any of it.
    assert_eq!(app.find(&w.bob, "apple").await.hits.len(), 1);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(rows, 4);
    finish(w).await;
}

#[sqlx::test]
async fn an_unusable_query_is_explained_and_a_bad_request_is_invalid(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let many_words = (0..=MAX_QUERY_TERMS)
        .map(|n| format!("w{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    let many_values = (0..=MAX_FILTER_VALUES)
        .map(|n| format!("from:u{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    let too_long = "a".repeat(MAX_QUERY_CHARS + 1);
    for q in [
        "",
        "   ",
        "... !!",
        "-draft",
        "🎉",
        "after:yesterday",
        "before:2026-13-45",
        "has:video",
        &too_long,
        &many_words,
        &many_values,
    ] {
        let reason = SearchQuery::parse(q).unwrap_err().to_string();
        let error = denied(
            app.search_raw(&w.bob, app.workspace_id, &[("q", q)]).await,
            ErrorCode::InvalidRequest,
        )
        .await;
        assert_eq!(error.message, reason, "{q:?}");
    }
    // The request itself: no q, a limit that is not a number, a sort that is not one.
    denied(
        app.search_raw(&w.bob, app.workspace_id, &[]).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    for bad in [("limit", "many"), ("offset", "-1"), ("sort", "sideways")] {
        denied(
            app.search_raw(&w.bob, app.workspace_id, &[("q", "x"), bad])
                .await,
            ErrorCode::InvalidRequest,
        )
        .await;
    }
    // Further than a query may page.
    let error = denied(
        app.search_raw(&w.bob, app.workspace_id, &[("q", "x"), ("offset", "201")])
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    assert!(error.message.contains("200"), "{}", error.message);
    // Sign-in is required.
    let response = app
        .http
        .get(app.url(&lazy_protocol::api::paths::search(app.workspace_id)))
        .query(&[("q", "x")])
        .send()
        .await
        .unwrap();
    denied(response, ErrorCode::Unauthenticated).await;
    finish(w).await;
}

// --- Who can find what -------------------------------------------------------------------------

#[sqlx::test]
async fn a_person_finds_what_is_in_their_conversations_and_in_public_channels_only(pool: PgPool) {
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
    let old = app.create_channel(alice, "old", "public").await;
    let dm = app.open(alice, &[bob]).await;
    let group = app.open(alice, &[bob, carol]).await;
    for (who, conversation, tag) in [
        (dave, app.general, "general"),
        (alice, open.id, "open"),
        (alice, secret.id, "secret"),
        (alice, old.id, "old"),
        (alice, dm.id, "dm"),
        (alice, group.id, "group"),
    ] {
        app.send(who, conversation, &format!("needle {tag}")).await;
    }
    assert!(
        app.act(alice, old.id, "archive")
            .await
            .status()
            .is_success()
    );
    // Another workspace's message, in a public channel of its own.
    let elsewhere = app.other_workspace(&[(alice, "member")]).await;
    let foreign = ConversationId::generate();
    sqlx::query(
        "INSERT INTO conversations (id, workspace_id, kind, name, visibility) \
         VALUES ($1, $2, 'channel', 'abroad', 'public')",
    )
    .bind(foreign.as_uuid())
    .bind(elsewhere.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, author_id, document, plain_text, seq, created_at) \
         VALUES ($1, $2, $3, '{\"version\":2,\"blocks\":[{\"type\":\"paragraph\",\"inlines\":[{\"type\":\"text\",\"text\":\"needle abroad\"}]}]}', 'needle abroad', 1, now())",
    )
    .bind(MessageId::generate().as_uuid())
    .bind(foreign.as_uuid())
    .bind(alice.user.id.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();

    let all = [
        "needle dm",
        "needle general",
        "needle group",
        "needle old",
        "needle open",
        "needle secret",
    ];
    let found = |who: &Account| {
        let who = who.clone();
        async move { sorted(texts(&app.find(&who, "needle").await)) }
    };
    assert_eq!(found(alice).await, all);
    assert_eq!(found(bob).await, all);
    assert_eq!(
        found(carol).await,
        [
            "needle general",
            "needle group",
            "needle old",
            "needle open"
        ]
    );
    assert_eq!(
        found(dave).await,
        ["needle general", "needle old", "needle open"]
    );

    // A hit in a public channel the person has not joined carries the channel (including an
    // archived one); a direct conversation's hit carries its participants.
    let hits = app.find(carol, "needle open").await.hits;
    assert_eq!(hits.len(), 1);
    assert_eq!(name_of(&hits[0]).as_deref(), Some("open"));
    assert_eq!(
        hits[0].conversation,
        app.conversations_of(alice)
            .await
            .into_iter()
            .find(|c| c.id == open.id)
            .unwrap()
    );
    let hits = app.find(carol, "needle old").await.hits;
    assert!(hits[0].conversation.archived);
    let hits = app.find(bob, "needle dm").await.hits;
    let mut people = vec![alice.user.id, bob.user.id];
    people.sort();
    assert_eq!(hits[0].conversation.participants, people);
    assert_eq!(hits[0].conversation.name, None);

    // `in:` cannot tell a private channel from one that does not exist.
    let hidden = app.find(carol, "needle in:secret").await;
    let missing = app.find(carol, "needle in:nowhere").await;
    assert_eq!(hidden, missing);
    assert!(hidden.hits.is_empty());
    assert_eq!(app.find(bob, "needle in:secret").await.hits.len(), 1);
    assert_eq!(
        app.find(carol, "in:secret").await,
        app.find(carol, "in:nowhere").await
    );
    assert_eq!(app.find(carol, "in:dm").await.hits.len(), 0);

    // Removed from a private channel: its messages are gone from their results at once.
    assert!(
        app.remove_member(alice, secret.id, bob)
            .await
            .status()
            .is_success()
    );
    assert_eq!(
        found(bob).await,
        [
            "needle dm",
            "needle general",
            "needle group",
            "needle old",
            "needle open"
        ]
    );
    // Added back: they return (a member sees the full history).
    assert!(
        app.add_members(alice, secret.id, &[bob])
            .await
            .status()
            .is_success()
    );
    assert_eq!(found(bob).await.len(), 6);

    // The other workspace's message is found there and only there.
    let response = app.search_raw(alice, elsewhere, &[("q", "needle")]).await;
    let abroad: SearchResponse = response.json().await.unwrap();
    assert_eq!(texts(&abroad), ["needle abroad"]);

    // Not a member of the workspace: nothing, and no sign that it exists.
    let outsider = app.register("outsider").await;
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
        .bind(outsider.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    denied(
        app.search_raw(&outsider, app.workspace_id, &[("q", "needle")])
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.search_raw(carol, WorkspaceId::generate(), &[("q", "needle")])
            .await,
        ErrorCode::NotFound,
    )
    .await;
    // A member removed from the workspace loses everything at once.
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1 AND workspace_id = $2")
        .bind(dave.user.id.as_uuid())
        .bind(app.workspace_id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    denied(
        app.search_raw(dave, app.workspace_id, &[("q", "needle")])
            .await,
        ErrorCode::NotFound,
    )
    .await;
    finish(w).await;
}

#[sqlx::test]
async fn a_hit_carries_the_message_as_a_history_page_does(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let file = app
        .upload(
            &w.alice,
            app.general,
            b"data".to_vec(),
            "application/zip",
            "data.zip",
        )
        .await;
    let sent = app
        .send_with_files(&w.alice, app.general, "parts needle", &[file.id])
        .await;
    app.react(&w.bob, sent.id, "👍", true).await;
    app.reply(&w.carol, &sent, "a reply").await;
    let found = app.find(&w.bob, "parts").await;
    let hit = &found.hits[0];
    assert_eq!(hit.message, app.get_message(&w.bob, sent.id).await);
    assert_eq!(hit.message.attachments.len(), 1);
    assert_eq!(hit.message.reactions.len(), 1);
    assert_eq!(hit.message.thread.as_ref().map(|t| t.reply_count), Some(1));
    // A mention is a part of the message too.
    let mentioned = app
        .send_mentioning(
            &w.alice,
            app.general,
            "hello @bob",
            &[("bob", MentionTarget::User { id: w.bob.user.id })],
        )
        .await;
    let hit = &app.find(&w.carol, "hello").await.hits[0];
    assert_eq!(hit.message.mentions, mentioned.mentions);
    assert_eq!(hit.message.mentions.users, vec![w.bob.user.id]);
    finish(w).await;
}

// --- Filters -----------------------------------------------------------------------------------

#[sqlx::test]
async fn from_and_in_narrow_by_person_and_channel_with_or_among_values(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let (alice, bob, carol) = (&w.alice, &w.bob, &w.carol);
    let design = app.create_channel(alice, "design", "public").await;
    let tools = app.create_channel(alice, "tools", "public").await;
    assert!(app.act(bob, design.id, "join").await.status().is_success());
    assert!(app.act(carol, tools.id, "join").await.status().is_success());
    app.send(alice, app.general, "report one").await;
    app.send(bob, app.general, "report two").await;
    app.send(bob, design.id, "report three").await;
    app.send(carol, tools.id, "report four").await;
    let f = |q: &'static str| async move { sorted(texts(&app.find(carol, q).await)) };
    assert_eq!(f("report from:alice").await, ["report one"]);
    assert_eq!(f("report from:@Alice").await, ["report one"]);
    assert_eq!(
        f("report from:alice from:bob").await,
        ["report one", "report three", "report two"]
    );
    assert!(f("report from:nobody").await.is_empty());
    assert!(f("report from:alice from:nobody").await.len() == 1);
    assert_eq!(f("report in:design").await, ["report three"]);
    assert_eq!(f("report in:#Design").await, ["report three"]);
    assert_eq!(
        f("report in:design in:tools").await,
        ["report four", "report three"]
    );
    assert_eq!(f("report in:general from:bob").await, ["report two"]);
    assert!(f("report in:design from:alice").await.is_empty());
    // Only filters: everything that matches, newest first.
    let found = app.find(carol, "from:bob").await;
    assert_eq!(texts(&found), ["report three", "report two"]);
    assert_eq!(
        texts(&app.find(carol, "in:general").await),
        ["report two", "report one"]
    );
    assert!(
        app.find(carol, "from:bob")
            .await
            .hits
            .iter()
            .all(|h| h.snippet.highlights.is_empty())
    );
    assert_eq!(
        app.find(carol, "from:bob").await.hits[0].snippet.text,
        "report three"
    );
    finish(w).await;
}

#[sqlx::test]
async fn dates_are_utc_days_with_after_inclusive_and_before_exclusive(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let stamps = [
        ("2026-09-30T23:59:59Z", "late september"),
        ("2026-10-01T00:00:00Z", "first midnight"),
        ("2026-10-01T23:59:59.999Z", "first evening"),
        ("2026-10-02T00:00:00Z", "second midnight"),
    ];
    for (stamp, text) in stamps {
        let m = app
            .send(&w.alice, app.general, &format!("day {text}"))
            .await;
        sqlx::query("UPDATE messages SET created_at = $2::timestamptz WHERE id = $1")
            .bind(m.id.as_uuid())
            .bind(stamp)
            .execute(&app.pool)
            .await
            .unwrap();
    }
    let bob = &w.bob;
    let f = |q: &'static str| async move { sorted(texts(&app.find(bob, q).await)) };
    assert_eq!(
        f("day after:2026-10-01").await,
        [
            "day first evening",
            "day first midnight",
            "day second midnight"
        ]
    );
    assert_eq!(
        f("day before:2026-10-02").await,
        [
            "day first evening",
            "day first midnight",
            "day late september"
        ]
    );
    assert_eq!(
        f("day after:2026-10-01 before:2026-10-02").await,
        ["day first evening", "day first midnight"]
    );
    assert_eq!(f("after:2026-10-02").await, ["day second midnight"]);
    assert!(f("after:2026-10-02 before:2026-10-02").await.is_empty());
    assert_eq!(f("before:2026-10-01").await, ["day late september"]);
    finish(w).await;
}

#[sqlx::test]
async fn has_file_and_has_image_look_at_non_deleted_attachments_by_type(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let picture = app
        .upload(
            &w.alice,
            app.general,
            png_bytes(40, 40, 255),
            "image/png",
            "photo.png",
        )
        .await;
    let archive = app
        .upload(
            &w.alice,
            app.general,
            b"zip bytes".to_vec(),
            "application/zip",
            "bundle.zip",
        )
        .await;
    app.send_with_files(&w.alice, app.general, "pic here", &[picture.id])
        .await;
    app.send_with_files(&w.alice, app.general, "doc here", &[archive.id])
        .await;
    let gone = app
        .send(&w.alice, app.general, "here but nothing attached")
        .await;
    let bob = &w.bob;
    let f = |q: &'static str| async move { sorted(texts(&app.find(bob, q).await)) };
    assert_eq!(f("has:file").await, ["doc here", "pic here"]);
    assert_eq!(f("has:image").await, ["pic here"]);
    assert_eq!(f("here has:image").await, ["pic here"]);
    assert_eq!(f("here has:file").await, ["doc here", "pic here"]);
    assert_eq!(f("here").await.len(), 3);
    assert_eq!(f("has:file from:alice").await.len(), 2);
    assert!(f("has:file from:bob").await.is_empty());
    // A message that has since been deleted does not match, nor does anything about its files.
    let deleted = app
        .send_with_files(
            &w.alice,
            app.general,
            "short lived",
            &[app
                .upload(
                    &w.alice,
                    app.general,
                    png_bytes(8, 8, 255),
                    "image/png",
                    "gone.png",
                )
                .await
                .id],
        )
        .await;
    assert_eq!(f("has:image").await.len(), 2);
    app.delete(&w.alice, deleted.id).await;
    assert_eq!(f("has:image").await, ["pic here"]);
    assert!(f("gone").await.is_empty());
    let _ = gone;
    finish(w).await;
}

// --- Files by name -----------------------------------------------------------------------------

#[sqlx::test]
async fn a_file_is_found_by_the_words_of_its_name_and_a_message_is_listed_once(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let alice = &w.alice;
    let pdf = |name: &'static str| async move {
        app.upload(
            alice,
            app.general,
            b"%PDF-1.4 x".to_vec(),
            "application/pdf",
            name,
        )
        .await
    };
    // A file whose name matches and whose message does not.
    let report = pdf("Quarterly_Report-v2.pdf").await;
    let sent = app
        .send_with_files(&w.alice, app.general, "see attached", &[report.id])
        .await;
    let found = app.find(&w.bob, "report").await;
    assert_eq!(found.hits.len(), 1);
    let hit = &found.hits[0];
    assert_eq!(
        hit.kind,
        SearchHitKind::Attachment {
            attachment_id: report.id
        }
    );
    assert_eq!(hit.snippet.text, "Quarterly_Report-v2.pdf");
    assert_eq!(marked(hit), ["Report"]);
    assert_eq!(hit.message.id, sent.id);
    assert_eq!(app.find(&w.bob, "quarterly v2").await.hits.len(), 1);
    assert_eq!(app.find(&w.bob, "pdf").await.hits.len(), 1);
    assert_eq!(app.find(&w.bob, "report -quarterly").await.hits.len(), 0);
    // Words in the message's own text and in a file's name: one hit, for the text.
    let both = pdf("notes_budget.pdf").await;
    app.send_with_files(&w.alice, app.general, "budget summary", &[both.id])
        .await;
    let found = app.find(&w.bob, "budget").await;
    assert_eq!(found.hits.len(), 1);
    assert_eq!(found.hits[0].kind, SearchHitKind::Message);
    // Listed once also means the page and `has_more` count it once.
    let one = app.find_with(&w.bob, "budget", &[("limit", "1")]).await;
    assert_eq!((one.hits.len(), one.has_more), (1, false));
    // Several files of one message whose names match: one hit, for the first of them.
    let (first, second) = (pdf("plan_alpha.pdf").await, pdf("plan_beta.pdf").await);
    app.send_with_files(
        &w.alice,
        app.general,
        "two attachments",
        &[first.id, second.id],
    )
    .await;
    let found = app.find(&w.bob, "plan").await;
    let files: Vec<_> = found
        .hits
        .iter()
        .filter(|h| h.snippet.text.starts_with("plan_"))
        .collect();
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].kind,
        SearchHitKind::Attachment {
            attachment_id: first.id
        }
    );
    // Once the first is gone (its message is not), the hit is for the next one.
    sqlx::query("UPDATE attachments SET deleted_at = now() WHERE id = $1")
        .bind(first.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    let found = app.find(&w.bob, "plan").await;
    let files: Vec<_> = found
        .hits
        .iter()
        .filter(|h| h.snippet.text.starts_with("plan_"))
        .collect();
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].kind,
        SearchHitKind::Attachment {
            attachment_id: second.id
        }
    );
    // A message whose only file is deleted has no file for `has:file` or for a name.
    let lonely = pdf("lonely_ledgerfile.pdf").await;
    app.send_with_files(&w.alice, app.general, "just that", &[lonely.id])
        .await;
    assert_eq!(app.find(&w.bob, "ledgerfile").await.hits.len(), 1);
    assert!(
        app.find(&w.bob, "just has:file")
            .await
            .hits
            .iter()
            .any(|h| h.snippet.text == "just that")
    );
    sqlx::query("UPDATE attachments SET deleted_at = now() WHERE id = $1")
        .bind(lonely.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    assert!(app.find(&w.bob, "ledgerfile").await.hits.is_empty());
    assert!(
        app.find(&w.bob, "just has:file")
            .await
            .hits
            .iter()
            .all(|h| h.snippet.text != "just that")
    );
    // Only words that start with a query word are marked in a file's name.
    let odd = pdf("export_port_xreport.pdf").await;
    app.send_with_files(&w.alice, app.general, "odd name", &[odd.id])
        .await;
    let found = app.find(&w.bob, "port").await;
    assert_eq!(found.hits.len(), 1);
    assert_eq!(marked(&found.hits[0]), ["port"]);
    // A file only some of whose words match does not satisfy a query that has others.
    assert!(app.find(&w.bob, "plan gamma").await.hits.is_empty());
    // Pending files (not on a message) are not found; neither are a private channel's by
    // someone outside it, nor a deleted message's.
    pdf("pending_unsent.pdf").await;
    assert!(app.find(&w.bob, "unsent").await.hits.is_empty());
    let secret = app.create_channel(&w.alice, "secret", "private").await;
    let hidden = app
        .upload(
            &w.alice,
            secret.id,
            b"%PDF-1.4 y".to_vec(),
            "application/pdf",
            "hidden_ledger.pdf",
        )
        .await;
    let in_secret = app
        .send_with_files(&w.alice, secret.id, "ledger inside", &[hidden.id])
        .await;
    assert!(app.find(&w.bob, "ledger").await.hits.is_empty());
    assert_eq!(app.find(&w.alice, "ledger").await.hits.len(), 1);
    app.delete(&w.alice, in_secret.id).await;
    assert!(app.find(&w.alice, "ledger").await.hits.is_empty());
    assert!(app.find(&w.alice, "hidden").await.hits.is_empty());
    // A file in a thread reply is found, and its message carries the thread root.
    let root = app.send(&w.alice, app.general, "thread root").await;
    let attached = pdf("reply_evidence.pdf").await;
    let reply = app
        .send_with_files_raw(
            &w.alice,
            app.general,
            lazy_domain::OperationId::generate(),
            "",
            &[attached.id],
            Some(&root),
            false,
        )
        .await;
    assert!(reply.status().is_success());
    let found = app.find(&w.bob, "evidence").await;
    assert_eq!(found.hits[0].message.thread_root, Some(root.id));
    let _: AttachmentId = attached.id;
    finish(w).await;
}

// --- Scripts and snippets ----------------------------------------------------------------------

#[sqlx::test]
async fn every_script_is_searchable_and_ranges_count_characters(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    for text in [
        "日本語のテスト",
        "café au lait",
        "cafe noir",
        "party \u{1f389}\u{1f389} time",
        "xe\u{301}y target",
        "مرحبا بالعالم",
        "שלום עולם",
        "ñandú señal",
    ] {
        app.send(&w.alice, app.general, text).await;
    }
    // CJK runs are one word each: a prefix finds the run, and the range covers it.
    let found = app.find(&w.bob, "日本").await;
    assert_eq!(found.hits.len(), 1);
    assert!(marked(&found.hits[0])[0].starts_with("日本"));
    // No accent folding: `cafe` is not `café`.
    assert_eq!(
        sorted(texts(&app.find(&w.bob, "caf").await)),
        ["cafe noir", "café au lait"]
    );
    assert_eq!(texts(&app.find(&w.bob, "cafe").await), ["cafe noir"]);
    assert_eq!(texts(&app.find(&w.bob, "café").await), ["café au lait"]);
    assert_eq!(texts(&app.find(&w.bob, "señal").await), ["ñandú señal"]);
    assert_eq!(app.find(&w.bob, "SEÑAL").await.hits.len(), 1);
    // Two emoji before the match: the range is in characters (bytes would give 15..19).
    let found = app.find(&w.bob, "time").await;
    let range = found.hits[0].snippet.highlights[0];
    assert_eq!((range.start, range.end), (9, 13));
    assert_eq!(marked(&found.hits[0]), ["time"]);
    // A combining mark is a character of its own: `x`, `e`, U+0301, `y`, space.
    let found = app.find(&w.bob, "target").await;
    let range = found.hits[0].snippet.highlights[0];
    assert_eq!((range.start, range.end), (5, 11));
    assert_eq!(marked(&found.hits[0]), ["target"]);
    // Right-to-left scripts are in logical order, as typed.
    let found = app.find(&w.bob, "مرحبا").await;
    assert_eq!(marked(&found.hits[0]), ["مرحبا"]);
    assert_eq!(found.hits[0].snippet.highlights[0].start, 0);
    let found = app.find(&w.bob, "עולם").await;
    let range = found.hits[0].snippet.highlights[0];
    assert_eq!((range.start, range.end), (5, 9));
    finish(w).await;
}

#[sqlx::test]
async fn a_snippet_is_a_short_excerpt_with_consistent_ranges(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let words: Vec<String> = (0..120)
        .map(|n| {
            if n == 60 {
                "needleword".to_owned()
            } else {
                format!("filler{n}")
            }
        })
        .collect();
    app.send(&w.alice, app.general, &words.join(" ")).await;
    let found = app.find(&w.bob, "needle").await;
    let hit = &found.hits[0];
    let count = hit.snippet.text.split_whitespace().count();
    assert!(
        (8..=24).contains(&count),
        "{count} words: {}",
        hit.snippet.text
    );
    assert!(hit.snippet.text.chars().count() < words.join(" ").chars().count());
    assert_eq!(marked(hit), ["needleword"]);
    // Every range lies inside the text, they are in order and do not overlap.
    let many = "alpha beta ".repeat(40);
    app.send(&w.alice, app.general, &many).await;
    for hit in &app.find(&w.bob, "alpha beta").await.hits {
        let mut end = 0;
        for range in &hit.snippet.highlights {
            assert!(range.start >= end && range.start < range.end);
            end = range.end;
        }
        assert!(end as usize <= hit.snippet.text.chars().count());
        assert!(marked(hit).iter().all(|m| m == "alpha" || m == "beta"));
    }
    // A short message is shown whole.
    app.send(&w.alice, app.general, "brief note").await;
    let hit = &app.find(&w.bob, "note").await.hits[0];
    assert_eq!(hit.snippet.text, "brief note");
    assert_eq!(marked(hit), ["note"]);
    finish(w).await;
}

// --- The index follows edits and deletions -----------------------------------------------------

#[sqlx::test]
async fn edits_reindex_and_deletions_remove_including_tombstone_roots(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let m = app.send(&w.alice, app.general, "alpha beta").await;
    assert_eq!(app.find(&w.bob, "alpha").await.hits.len(), 1);
    let edited = app.edit(&w.alice, &m, "gamma delta").await;
    assert!(app.find(&w.bob, "alpha").await.hits.is_empty());
    let found = app.find(&w.bob, "gamma").await;
    assert_eq!(ids(&found), [m.id]);
    assert_eq!(found.hits[0].message.revision, edited.revision);
    app.delete(&w.alice, m.id).await;
    assert!(app.find(&w.bob, "gamma").await.hits.is_empty());
    assert!(app.find(&w.bob, "from:alice").await.hits.is_empty());

    // A deleted root with replies is not a result; its replies are, and open its thread.
    let root = app.send(&w.alice, app.general, "rootword launch").await;
    assert_eq!(
        app.find(&w.bob, "launch").await.hits[0].message.thread,
        None,
        "no replies yet"
    );
    let r1 = app.reply(&w.bob, &root, "replyword one").await;
    let r2 = app.reply_in_channel(&w.carol, &root, "replyword two").await;
    let gone = app.reply(&w.carol, &root, "replyword three").await;
    let found = app.find(&w.dave, "launch").await;
    assert_eq!(
        found.hits[0].message.thread.as_ref().map(|t| t.reply_count),
        Some(3)
    );
    app.delete(&w.carol, gone.id).await;
    let found = app.find(&w.dave, "replyword").await;
    assert_eq!(sorted(texts(&found)), ["replyword one", "replyword two"]);
    assert!(
        found
            .hits
            .iter()
            .all(|h| h.message.thread_root == Some(root.id))
    );
    assert_eq!(
        found
            .hits
            .iter()
            .find(|h| h.message.id == r2.id)
            .map(|h| h.message.in_channel),
        Some(true)
    );
    app.delete(&w.alice, root.id).await;
    assert!(app.find(&w.dave, "rootword").await.hits.is_empty());
    assert!(app.find(&w.dave, "launch").await.hits.is_empty());
    let found = app.find(&w.dave, "replyword").await;
    assert_eq!(ids(&found).len(), 2);
    assert!(ids(&found).contains(&r1.id));
    assert!(
        app.find(&w.dave, "in:general")
            .await
            .hits
            .iter()
            .all(|h| !h.message.deleted)
    );
    // Even a deleted row that still held its text (the server empties it, but the guard does not
    // rely on that) is not a result, by text or by filters.
    let ghost = app.send(&w.alice, app.general, "ghostly note").await;
    sqlx::query("UPDATE messages SET deleted = true, listed = false WHERE id = $1")
        .bind(ghost.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    assert!(app.find(&w.dave, "ghostly").await.hits.is_empty());
    assert!(!ids(&app.find(&w.dave, "from:alice").await).contains(&ghost.id));
    finish(w).await;
}

#[sqlx::test]
async fn mentions_emoji_and_bots_are_text_like_any_other(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    app.send_mentioning(
        &w.alice,
        app.general,
        "ping @bob about it",
        &[("bob", MentionTarget::User { id: w.bob.user.id })],
    )
    .await;
    assert_eq!(
        texts(&app.find(&w.carol, "@bob").await),
        ["@bob about it"].map(|_| "ping @bob about it".to_owned())
    );
    assert_eq!(app.find(&w.carol, "bob").await.hits.len(), 1);

    let parrot = app
        .upload_emoji(
            &w.alice,
            app.workspace_id,
            "party_parrot",
            png_bytes(30, 30, 255),
        )
        .await;
    let document: lazy_domain::MessageDocument = serde_json::from_value(serde_json::json!({
        "version": 4,
        "blocks": [{"type": "paragraph", "inlines": [
            {"type": "text", "text": "ship it "},
            {"type": "emoji", "id": parrot.id, "name": parrot.name},
        ]}],
    }))
    .unwrap();
    let sent = app.send_doc_raw(&w.alice, app.general, document).await;
    assert!(sent.status().is_success());
    let found = app.find(&w.carol, ":party_parrot:").await;
    assert_eq!(found.hits.len(), 1);
    assert_eq!(marked(&found.hits[0]), ["party", "parrot"]);
    assert_eq!(app.find(&w.carol, "party_parrot").await.hits.len(), 1);
    assert_eq!(app.find(&w.carol, "parrot").await.hits.len(), 1);

    // A webhook's message is found, by its text and by its bot.
    let hook = app.create_webhook(&w.alice, app.general, "Deploybot").await;
    let id = app.deliver(&hook, "nightly build passed").await;
    let found = app.find(&w.carol, "nightly").await;
    assert_eq!(ids(&found), [id]);
    let bot: String = sqlx::query_scalar("SELECT username FROM users WHERE id = $1")
        .bind(found.hits[0].message.author_id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(ids(&app.find(&w.carol, &format!("from:{bot}")).await), [id]);
    finish(w).await;
}

// --- Order, caps and pages ---------------------------------------------------------------------

#[sqlx::test]
async fn relevance_prefers_the_closer_match_and_ties_go_to_the_newest(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let filler = "other words here ".repeat(10);
    let short = app.send(&w.alice, app.general, "rollout").await;
    let long = app
        .send(&w.alice, app.general, &format!("rollout {filler}"))
        .await;
    let twin_old = app.send(&w.alice, app.general, "rollout again").await;
    let twin_new = app.send(&w.alice, app.general, "rollout again").await;
    let relevance = app.find(&w.bob, "rollout").await;
    assert_eq!(
        relevance.hits[0].message.id, short.id,
        "the shortest, exact message ranks first"
    );
    assert_eq!(
        *ids(&relevance).last().unwrap(),
        long.id,
        "the diluted one last"
    );
    let position = |id| ids(&relevance).iter().position(|m| *m == id).unwrap();
    assert!(
        position(twin_new.id) < position(twin_old.id),
        "equal rank: newest first"
    );
    let recent = app
        .find_with(&w.bob, "rollout", &[("sort", "recent")])
        .await;
    assert_eq!(ids(&recent), [twin_new.id, twin_old.id, long.id, short.id]);
    // Only filters is always newest first, whatever the sort asked.
    let found = app
        .find_with(&w.bob, "from:alice", &[("sort", "relevance")])
        .await;
    assert_eq!(ids(&found), [twin_new.id, twin_old.id, long.id, short.id]);
    finish(w).await;
}

#[sqlx::test]
async fn ranking_looks_only_at_the_newest_candidates_and_says_so(pool: PgPool) {
    let app = App::start_with_search(
        pool,
        SearchOptions {
            rank_candidates: 3,
            ..SearchOptions::default()
        },
    )
    .await;
    let w = world_with(app).await;
    let app = &w.app;
    for n in 0..5 {
        app.send(&w.alice, app.general, &format!("match number {n}"))
            .await;
    }
    let found = app.find(&w.bob, "match").await;
    assert!(found.truncated);
    assert_eq!(found.hits.len(), 3);
    assert_eq!(
        sorted(texts(&found)),
        ["match number 2", "match number 3", "match number 4"]
    );
    assert!(!found.has_more);
    // Recent order does not rank, so it never truncates, and reaches all five.
    let recent = app.find_with(&w.bob, "match", &[("sort", "recent")]).await;
    assert!(!recent.truncated);
    assert_eq!(recent.hits.len(), 5);
    // Exactly at the cap is not truncated; a filter narrows below it.
    assert!(!app.find(&w.bob, "match number 4").await.truncated);
    assert!(!app.find(&w.bob, "match from:carol").await.truncated);
    sqlx::query("DELETE FROM messages WHERE plain_text IN ('match number 0', 'match number 1')")
        .execute(&app.pool)
        .await
        .unwrap();
    let found = app.find(&w.bob, "match").await;
    assert_eq!((found.hits.len(), found.truncated), (3, false));
    finish(w).await;
}

#[sqlx::test]
async fn pages_are_bounded_and_a_query_cannot_page_past_two_hundred(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    app.seed_messages(app.general, &w.alice, 230, |n| format!("pagetest {n}"))
        .await;
    let page = |limit: &'static str, offset: String| {
        let w = &w;
        async move {
            app.find_with(&w.bob, "pagetest", &[("limit", limit), ("offset", &offset)])
                .await
        }
    };
    // Default and clamped sizes.
    assert_eq!(app.find(&w.bob, "pagetest").await.hits.len(), 20);
    assert_eq!(page("0", "0".into()).await.hits.len(), 1);
    assert_eq!(page("1000", "0".into()).await.hits.len(), 50);
    // Pages do not overlap, run newest first for equal rank, and stop at 200.
    let mut seen = Vec::new();
    for (offset, has_more) in [(0, true), (50, true), (100, true), (150, false)] {
        let found = page("50", offset.to_string()).await;
        assert_eq!(found.hits.len(), 50, "offset {offset}");
        assert_eq!(found.has_more, has_more, "offset {offset}");
        assert!(!found.truncated);
        seen.extend(ids(&found));
    }
    assert_eq!(seen.len(), 200);
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 200);
    let first = app.get_message(&w.bob, seen[0]).await;
    let last = app.get_message(&w.bob, seen[199]).await;
    assert!(first.seq > last.seq, "newest first");
    // Beyond the reach: empty with nothing more; past the cap: invalid.
    let past = page("50", "200".into()).await;
    assert_eq!((past.hits.len(), past.has_more), (0, false));
    assert_eq!(page("50", "190".into()).await.hits.len(), 10);
    denied(
        app.search_raw(
            &w.bob,
            app.workspace_id,
            &[("q", "pagetest"), ("offset", "201")],
        )
        .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    // Short result sets report no more, in either order.
    let few = app
        .find_with(&w.bob, "pagetest 22", &[("limit", "50")])
        .await;
    assert!(!few.has_more);
    let recent = app
        .find_with(
            &w.bob,
            "pagetest",
            &[("sort", "recent"), ("limit", "50"), ("offset", "150")],
        )
        .await;
    assert_eq!((recent.hits.len(), recent.has_more), (50, false));
    let filters = app
        .find_with(&w.bob, "from:alice", &[("limit", "50"), ("offset", "100")])
        .await;
    assert_eq!((filters.hits.len(), filters.has_more), (50, true));
    finish(w).await;
}

// --- Cost --------------------------------------------------------------------------------------

#[sqlx::test]
async fn a_slow_search_is_cancelled_and_answers_unavailable_without_holding_anything(pool: PgPool) {
    let app = App::start_with_search(
        pool,
        SearchOptions {
            statement_timeout: Duration::from_millis(100),
            test_delay: Some(Duration::from_millis(1500)),
            ..SearchOptions::default()
        },
    )
    .await;
    let w = world_with(app).await;
    let app = &w.app;
    app.send(&w.alice, app.general, "something to find").await;
    let started = std::time::Instant::now();
    let error = denied(
        app.search_raw(&w.bob, app.workspace_id, &[("q", "something")])
            .await,
        ErrorCode::Unavailable,
    )
    .await;
    assert_eq!(
        error.message,
        "that search took too long; narrow it with in: or from:"
    );
    assert!(
        started.elapsed() < Duration::from_millis(1400),
        "cancelled by the limit, not run out"
    );
    // Many at once, and the server (and its pool) is still fine afterwards.
    let responses =
        join_all((0..12).map(|_| app.search_raw(&w.bob, app.workspace_id, &[("q", "something")])))
            .await;
    assert!(responses.iter().all(|r| r.status().as_u16() == 503));
    assert_eq!(
        app.send(&w.bob, app.general, "still working")
            .await
            .author_id,
        w.bob.user.id
    );
    // Other kinds of failure are not mistaken for it: a bad query is still a bad query.
    denied(
        app.search_raw(&w.bob, app.workspace_id, &[("q", "")]).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    finish(w).await;
}

#[sqlx::test]
async fn searching_while_messages_are_sent_edited_and_deleted_is_always_consistent(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let base = app.send(&w.alice, app.general, "churn start").await;
    let writers = async {
        let mut mine: Vec<Message> = Vec::new();
        for n in 0..25 {
            let m = app
                .send(&w.alice, app.general, &format!("churn message {n}"))
                .await;
            if n % 3 == 0 {
                app.edit(&w.alice, &m, &format!("changed words {n}")).await;
            } else if n % 3 == 1 {
                app.delete(&w.alice, m.id).await;
            }
            mine.push(m);
        }
        mine.len()
    };
    let searchers = join_all((0..3).map(|_| async {
        for _ in 0..25 {
            let response = app
                .search_raw(&w.bob, app.workspace_id, &[("q", "churn")])
                .await;
            assert_eq!(response.status().as_u16(), 200);
            let found: SearchResponse = response.json().await.unwrap();
            for hit in &found.hits {
                // One snapshot: every hit is a live message that has the word, as the hit shows it.
                assert!(!hit.message.deleted);
                assert!(
                    hit.message
                        .document
                        .plain_text()
                        .to_lowercase()
                        .contains("churn"),
                    "{:?}",
                    hit.message.document.plain_text()
                );
                assert!(hit.snippet.text.to_lowercase().contains("churn"));
                assert_eq!(marked(hit).len(), hit.snippet.highlights.len());
            }
        }
    }));
    let (sent, _) = tokio::join!(writers, searchers);
    assert_eq!(sent, 25);
    let final_hits = app.find(&w.bob, "churn").await;
    // 25 sent: a third edited away, a third deleted, a third left; plus the first.
    assert_eq!(final_hits.hits.len(), 1 + 8);
    assert!(ids(&final_hits).contains(&base.id));
    finish(w).await;
}

#[sqlx::test]
async fn the_server_speaks_a_protocol_that_has_search(pool: PgPool) {
    const { assert!(PROTOCOL_VERSION >= 10) };
    let w = world(pool).await;
    // Every search shape answers with the same response type.
    let found: SearchResponse = w
        .app
        .search_raw(&w.bob, w.app.workspace_id, &[("q", "anything")])
        .await
        .json()
        .await
        .unwrap();
    assert!(found.hits.is_empty());
    finish(w).await;
}
