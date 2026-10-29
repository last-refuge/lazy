//! Phase 2E end to end: the app reopens where the person left it. Real client stacks with their
//! real profile files, a real server, and real Postgres.

mod support;

use lazy_client_core::{Input, TextSelection};
use lazy_client_runtime::MemoryStorage;
use lazy_domain::{ChannelVisibility, ConversationId};
use sqlx::PgPool;
use support::{Client, World};

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_restarted_client_reopens_the_conversation_and_the_caret_where_they_were(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("alice");
    let secrets = MemoryStorage::new();
    let alice = Client::start("alice", &world.server.base_url, &profile, secrets.clone());
    alice.register("alice").await;
    let general: ConversationId = alice.read(|core| core.selected_conversation().unwrap());

    alice.act(Input::CreateChannel {
        name: "design".to_owned(),
        visibility: ChannelVisibility::Public,
        topic: String::new(),
        description: String::new(),
    });
    alice
        .wait_until("the new channel", |core| {
            core.conversations().iter().any(|c| !c.is_default)
        })
        .await;
    let design = alice
        .read(|core| {
            core.conversations()
                .iter()
                .find(|c| !c.is_default)
                .map(|c| c.id)
        })
        .unwrap();
    alice
        .wait_until("design selected", move |core| {
            core.selected_conversation() == Some(design)
        })
        .await;

    // A draft in each conversation, with a caret in the middle of one and a selected word in the
    // other (the text has a combining mark and an emoji, so the offsets must stay on boundaries).
    let text = "cafe\u{301} ☕ plans for friday";
    alice.act(Input::SetDraft {
        conversation: design,
        text: text.to_owned(),
    });
    alice.act(Input::SetDraftSelection {
        conversation: design,
        anchor: 14,
        head: 14,
    });
    alice.act(Input::SetDraft {
        conversation: general,
        text: "hello everyone".to_owned(),
    });
    alice.act(Input::SetDraftSelection {
        conversation: general,
        anchor: 6,
        head: 14,
    });
    alice.stop().await;

    // Reopened with no network at all, it is on the design channel with both drafts and carets.
    let alice = Client::start("alice", &world.server.base_url, &profile, secrets);
    alice
        .wait_until("the saved view", move |core| {
            core.selected_conversation() == Some(design)
        })
        .await;
    assert_eq!(alice.read(|core| core.draft(design).to_owned()), text);
    assert_eq!(
        alice.read(|core| core.draft_selection(design)),
        Some(TextSelection {
            anchor: 14,
            head: 14
        })
    );
    assert_eq!(
        alice.read(|core| core.draft_selection(general)),
        Some(TextSelection {
            anchor: 6,
            head: 14
        })
    );
    // The caret never lands inside a grapheme: it sits after "cafe\u{301} " even if asked for
    // the byte between the letter and its accent.
    alice.act(Input::SetDraftSelection {
        conversation: design,
        anchor: 4,
        head: 4,
    });
    let sel = alice.read(|core| core.draft_selection(design)).unwrap();
    assert!(
        text.is_char_boundary(sel.anchor) && sel.anchor != 4,
        "{sel:?}"
    );

    alice.stop().await;
    world.server.shutdown().await.unwrap();
}
