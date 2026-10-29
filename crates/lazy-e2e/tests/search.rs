//! Phase 3C end to end: search across real clients, a real server, and real Postgres.

mod support;

use lazy_client_core::{ClientCore, FileSource, Input, LocalFile, SearchStatus};
use lazy_client_runtime::MemoryStorage;
use lazy_domain::{ChannelVisibility, ConversationId};
use lazy_protocol::{SearchHit, SearchHitKind, SearchSort};
use sqlx::PgPool;
use support::{Client, World};

fn start(world: &World, dir: &std::path::Path, name: &'static str) -> Client {
    Client::start(
        name,
        &world.server.base_url,
        &dir.join(name),
        MemoryStorage::new(),
    )
}

fn general(client: &Client) -> ConversationId {
    client.read(|core| {
        core.conversations()
            .iter()
            .find(|c| c.is_default)
            .map(|c| c.id)
            .expect("the default channel")
    })
}

async fn create_channel(
    client: &Client,
    name: &str,
    visibility: ChannelVisibility,
) -> ConversationId {
    client.act(Input::CreateChannel {
        name: name.to_owned(),
        visibility,
        topic: String::new(),
        description: String::new(),
    });
    let wanted = name.to_owned();
    client
        .wait_until("the channel", move |core| {
            core.conversations()
                .iter()
                .any(|c| c.name.as_ref().is_some_and(|n| n.as_str() == wanted))
        })
        .await;
    let id = client
        .read(|core| {
            core.conversations()
                .iter()
                .find(|c| c.name.as_ref().is_some_and(|n| n.as_str() == name))
                .map(|c| c.id)
        })
        .unwrap();
    client
        .wait_until("the channel to be selected", move |core| {
            core.selected_conversation() == Some(id)
        })
        .await;
    id
}

/// Types a query and runs it, as the screen does after its pause, and waits for the answer.
async fn search(client: &Client, text: &str) -> Vec<SearchHit> {
    client.act(Input::OpenSearch);
    client.act(Input::SetSearchQuery {
        text: text.to_owned(),
    });
    client.act(Input::RunSearch);
    let wanted = text.to_owned();
    client
        .wait_until("the answer", move |core| {
            core.search().is_some_and(|v| {
                v.query == wanted
                    && matches!(v.status, SearchStatus::Ready | SearchStatus::Failed { .. })
            })
        })
        .await;
    client.read(|core| core.search().unwrap().hits.clone())
}

fn texts(hits: &[SearchHit]) -> Vec<String> {
    hits.iter()
        .map(|h| h.message.document.plain_text())
        .collect()
}

fn highlighted(hit: &SearchHit) -> Vec<String> {
    let chars: Vec<char> = hit.snippet.text.chars().collect();
    hit.snippet
        .highlights
        .iter()
        .map(|r| chars[r.start as usize..r.end as usize].iter().collect())
        .collect()
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_search_finds_what_the_person_can_read_and_nothing_else(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);

    alice.send("the deploy to staging failed");
    alice.send("lunch plans for friday");
    alice.send("quarterly roadmap draft");
    alice.wait_confirmed("quarterly roadmap draft").await;
    // A private channel only Alice is in, and a public one Bob has not joined.
    let secret = create_channel(&alice, "secret", ChannelVisibility::Private).await;
    alice.send("deploy secrets rotation");
    alice.wait_confirmed("deploy secrets rotation").await;
    let design = create_channel(&alice, "design", ChannelVisibility::Public).await;
    alice.send("deploy design mockups");
    alice.wait_confirmed("deploy design mockups").await;
    bob.wait_for_text("quarterly roadmap draft").await;

    // Bob finds the public things, never the private one; the match is highlighted by characters.
    let hits = search(&bob, "deploy").await;
    let found = texts(&hits);
    assert!(
        found.contains(&"the deploy to staging failed".to_owned()),
        "{found:?}"
    );
    assert!(
        found.contains(&"deploy design mockups".to_owned()),
        "{found:?}"
    );
    assert!(
        !found.contains(&"deploy secrets rotation".to_owned()),
        "{found:?}"
    );
    for hit in &hits {
        assert_eq!(highlighted(hit), ["deploy"], "{hit:?}");
    }
    // A prefix finds the same; a phrase needs the words together; an exclusion removes.
    assert_eq!(texts(&search(&bob, "deplo").await).len(), 2);
    assert_eq!(
        texts(&search(&bob, "\"lunch plans\"").await),
        ["lunch plans for friday"]
    );
    assert!(search(&bob, "\"plans lunch\"").await.is_empty());
    assert!(search(&bob, "roadmap -draft").await.is_empty());
    assert_eq!(
        texts(&search(&bob, "roadmap").await),
        ["quarterly roadmap draft"]
    );
    // Filters narrow it.
    assert_eq!(
        texts(&search(&bob, "deploy in:design").await),
        ["deploy design mockups"]
    );
    assert_eq!(
        texts(&search(&bob, "from:alice lunch").await),
        ["lunch plans for friday"]
    );
    assert!(search(&bob, "from:nobody lunch").await.is_empty());
    assert!(
        search(&bob, "deploy in:secret").await.is_empty(),
        "no hint that it exists"
    );

    // The hit in a channel Bob has not joined says so, and opening it offers to join.
    let hits = search(&bob, "mockups").await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].conversation.id, design);
    assert!(bob.read(|core| !core.search_hit_joined(&hits[0])));
    assert_eq!(bob.read(|core| core.search_hit_title(&hits[0])), "#design");
    bob.act(Input::OpenSearchHit(0));
    bob.wait_until("the join offer", move |core| {
        core.join_offer() == Some(design)
    })
    .await;

    // Alice, who is in the private channel, finds all three.
    assert_eq!(texts(&search(&alice, "deploy").await).len(), 3);

    // Editing reindexes, deleting removes.
    let (message, edited) = alice.read(|core| {
        let item = core
            .timeline(general)
            .iter()
            .find(|i| i.document.plain_text() == "the deploy to staging failed")
            .unwrap();
        (item.message_id().unwrap(), item.clone())
    });
    assert!(edited.is_confirmed());
    alice.act(Input::EditMessage {
        conversation: general,
        message,
        text: "the release to staging failed".to_owned(),
    });
    alice
        .wait_until("the edit", move |core| {
            core.timeline(general)
                .iter()
                .any(|i| i.message_id() == Some(message) && i.is_edited())
        })
        .await;
    let found = texts(&search(&bob, "deploy").await);
    assert_eq!(found, ["deploy design mockups"]);
    assert_eq!(
        texts(&search(&bob, "release").await),
        ["the release to staging failed"]
    );
    alice.act(Input::DeleteMessage {
        conversation: general,
        message,
    });
    alice
        .wait_until("the deletion", move |core| {
            core.timeline(general)
                .iter()
                .all(|i| i.message_id() != Some(message))
        })
        .await;
    assert!(search(&bob, "release").await.is_empty());

    // Added to the private channel, Bob finds its message; removed, he does not.
    let bob_id = bob.read(|core| core.me().unwrap().id);
    alice.act(Input::AddMembers {
        id: secret,
        users: vec![bob_id],
    });
    bob.wait_until("to be added", move |core| {
        core.conversation(secret).is_some()
    })
    .await;
    assert_eq!(
        texts(&search(&bob, "rotation").await),
        ["deploy secrets rotation"]
    );
    alice.act(Input::RemoveMember {
        id: secret,
        user: bob_id,
    });
    bob.wait_until("to be removed", move |core| {
        core.conversation(secret).is_none()
    })
    .await;
    assert!(search(&bob, "rotation").await.is_empty());

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn opening_a_hit_far_back_shows_the_message_and_a_long_result_list_pages(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);

    alice.send("remember the xyzzy incantation");
    alice.wait_confirmed("remember the xyzzy incantation").await;
    for n in 0..60 {
        alice.send(&format!("paginate filler {n}"));
    }
    alice.wait_confirmed("paginate filler 59").await;

    // Twenty-five matches: a page of twenty, then the rest.
    let hits = search(&bob, "paginate").await;
    assert_eq!(hits.len(), 20);
    assert!(bob.read(|core| core.search().unwrap().has_more));
    bob.act(Input::LoadMoreSearch);
    bob.wait_until("the next page", |core| {
        core.search()
            .is_some_and(|v| v.hits.len() == 40 && matches!(v.status, SearchStatus::Ready))
    })
    .await;
    bob.act(Input::LoadMoreSearch);
    bob.wait_until("the last page", |core| {
        core.search()
            .is_some_and(|v| v.hits.len() == 60 && !v.has_more)
    })
    .await;
    // Sorted by date, the newest comes first.
    bob.act(Input::SetSearchSort(SearchSort::Recent));
    bob.wait_until("the new order", |core| {
        core.search().is_some_and(|v| {
            v.sort == SearchSort::Recent
                && matches!(v.status, SearchStatus::Ready)
                && v.hits
                    .first()
                    .is_some_and(|h| h.message.document.plain_text() == "paginate filler 59")
        })
    })
    .await;

    // A newcomer's client holds only the newest page. A hit far back opens the conversation at the
    // message, paging as a permalink does.
    let carol = start(&world, dir.path(), "carol");
    carol.register("carol").await;
    let hits = search(&carol, "xyzzy").await;
    assert_eq!(hits.len(), 1);
    let target = hits[0].message.id;
    assert!(
        carol.read(|core| core
            .timeline(general)
            .iter()
            .all(|i| i.message_id() != Some(target))),
        "the newest page does not reach it"
    );
    carol.act(Input::OpenSearchHit(0));
    carol
        .wait_until("the jump target", move |core| {
            core.jump_target().is_some_and(|t| {
                t.conversation == general && t.key == lazy_client_core::ItemKey::Message(target)
            })
        })
        .await;
    assert!(carol.read(|core| {
        core.timeline(general)
            .iter()
            .any(|i| i.message_id() == Some(target))
    }));

    alice.stop().await;
    bob.stop().await;
    carol.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn any_script_and_file_names_are_found_and_bad_queries_ask_for_nothing(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);

    alice.send("日本語のテスト 🎉 café résumé");
    alice.wait_confirmed("日本語のテスト 🎉 café résumé").await;
    // Highlight ranges count characters: they must land on the matched text even after emoji.
    let hits = search(&bob, "café").await;
    assert_eq!(hits.len(), 1);
    assert_eq!(highlighted(&hits[0]), ["café"]);
    let hits = search(&bob, "日本語のテスト").await;
    assert_eq!(highlighted(&hits[0]), ["日本語のテスト"]);
    // There is no accent folding: `cafe` is not `café` (documented).
    assert!(search(&bob, "cafe").await.is_empty());

    // A file is found by its name, as a file hit, and has:file narrows to files.
    let path = dir.path().join("Quarterly_Report-v2.pdf");
    std::fs::write(&path, b"%PDF-1.7 not really").unwrap();
    let target = lazy_client_core::ComposerTarget::Conversation(general);
    alice.act(Input::AttachFiles {
        target,
        files: vec![LocalFile {
            source: FileSource::Path(path),
            name: "Quarterly_Report-v2.pdf".to_owned(),
        }],
    });
    alice
        .wait_until("the file to upload", move |core| {
            core.composer_attachments(target)
                .iter()
                .all(|c| matches!(c.state, lazy_client_core::AttachmentState::Uploaded))
                && !core.composer_attachments(target).is_empty()
        })
        .await;
    alice.send("see the numbers");
    alice.wait_confirmed("see the numbers").await;
    let hits = search(&bob, "report").await;
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(matches!(hits[0].kind, SearchHitKind::Attachment { .. }));
    assert_eq!(hits[0].snippet.text, "Quarterly_Report-v2.pdf");
    assert_eq!(texts(&search(&bob, "has:file").await), ["see the numbers"]);
    assert!(search(&bob, "has:image numbers").await.is_empty());

    // A query that cannot be used is explained at once and nothing is sent.
    bob.act(Input::SetSearchQuery {
        text: "after:yesterday".to_owned(),
    });
    let before = bob.effects().len();
    bob.act(Input::RunSearch);
    assert!(bob.read(|core| core.search().unwrap().parsed.is_err()));
    assert_eq!(
        bob.effects().len(),
        before,
        "no request for a query that does not parse"
    );
    let _: &ClientCore;

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}
