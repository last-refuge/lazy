//! Phase 3B end to end: custom emoji across real clients, a real server, and real Postgres.

mod support;

use lazy_client_core::{
    ActionKind, ActionState, ClientCore, FileSource, ImageState, Input, LocalFile,
};
use lazy_client_runtime::MemoryStorage;
use lazy_domain::{ConversationId, EmojiId, Inline, MentionDirectory, ReactionEmoji};
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

fn png(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 200])
    });
    let mut bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

fn file(dir: &std::path::Path, name: &str, bytes: &[u8]) -> LocalFile {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    LocalFile {
        source: FileSource::Path(path),
        name: name.to_owned(),
    }
}

fn live_named(core: &ClientCore, name: &str) -> Option<EmojiId> {
    core.live_custom_emoji()
        .find(|e| e.name == name)
        .map(|e| e.id)
}

async fn add(client: &Client, name: &str, file: LocalFile) -> EmojiId {
    client.act(Input::AddCustomEmoji {
        name: name.to_owned(),
        file,
    });
    let wanted = name.to_owned();
    client
        .wait_until("the emoji to exist", move |core| {
            live_named(core, &wanted).is_some()
        })
        .await;
    client.read(|core| live_named(core, name)).unwrap()
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn an_admin_adds_an_emoji_everyone_can_use_in_messages_and_reactions(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);

    // Only an admin adds emoji.
    assert!(alice.read(|core| core.can_manage_emoji()));
    assert!(bob.read(|core| !core.can_manage_emoji()));
    bob.act(Input::AddCustomEmoji {
        name: "sneaky".to_owned(),
        file: file(dir.path(), "sneaky.png", &png(32, 32)),
    });
    bob.wait_until("a refusal", |core| {
        matches!(
            core.action(ActionKind::AddCustomEmoji),
            ActionState::Failed(_)
        )
    })
    .await;

    // A wide picture is stored small, with its aspect kept, and everyone sees the new emoji.
    let id = add(
        &alice,
        "party_parrot",
        file(dir.path(), "parrot.png", &png(400, 200)),
    )
    .await;
    bob.wait_until("the emoji to reach Bob", move |core| {
        core.custom_emoji_by_id(id).is_some_and(|e| !e.deleted)
    })
    .await;
    bob.act(Input::LoadEmojiImage(id));
    bob.wait_until("the emoji image", move |core| {
        matches!(core.emoji_image(id), Some(ImageState::Ready(_)))
    })
    .await;
    let Some(ImageState::Ready(data)) = bob.read(|core| core.emoji_image(id).cloned()) else {
        unreachable!()
    };
    let stored = image::load_from_memory(data.bytes()).unwrap();
    assert_eq!((stored.width(), stored.height()), (128, 64));
    assert!(data.len() < 20_000);

    // `:party_parrot:` in a message becomes the emoji; a standard shortcode still works.
    alice
        .wait_until("the emoji in the directory", move |core| {
            core.mention_directory(general)
                .resolve_emoji("party_parrot")
                .is_some()
        })
        .await;
    alice.send("ship it :party_parrot: and :tada:");
    bob.wait_for_text("ship it :party_parrot: and 🎉").await;
    let item = bob.read(|core| {
        core.timeline(general)
            .iter()
            .find(|i| i.document.plain_text() == "ship it :party_parrot: and 🎉")
            .cloned()
            .unwrap()
    });
    assert_eq!(item.document.custom_emoji(), [id]);
    assert!(matches!(
        &item.document.blocks()[0],
        lazy_domain::Block::Paragraph { inlines }
            if inlines.iter().any(|i| matches!(i, Inline::Emoji { id: found, .. } if *found == id))
    ));
    // Editing shows the source again, with the name.
    alice.wait_confirmed("ship it :party_parrot: and 🎉").await;
    let message = item.message_id().unwrap();
    assert_eq!(
        alice.read(|core| core.edit_text(general, message)),
        Some("ship it :party_parrot: and 🎉".to_owned())
    );

    // Bob reacts with it; Alice sees the chip, named by identity.
    bob.act(Input::ToggleReaction {
        conversation: general,
        message,
        emoji: ReactionEmoji::custom(id),
    });
    alice
        .wait_until("Bob's custom reaction", move |core| {
            core.timeline(general).iter().any(|i| {
                i.message_id() == Some(message)
                    && i.reactions
                        .iter()
                        .any(|r| r.emoji == ReactionEmoji::custom(id))
            })
        })
        .await;

    // Renaming changes how it reads and disturbs nothing.
    alice.act(Input::RenameCustomEmoji {
        id,
        name: "parrot_party".to_owned(),
    });
    bob.wait_until("the rename", move |core| {
        core.custom_emoji_label(id) == Some("parrot_party")
    })
    .await;
    assert!(bob.read(|core| {
        core.timeline(general)
            .iter()
            .any(|i| i.document.custom_emoji() == [id])
    }));

    // Deleting leaves the name to show, refuses new uses, and still lets a reaction be withdrawn.
    alice.act(Input::DeleteCustomEmoji(id));
    bob.wait_until("the deletion", move |core| {
        core.custom_emoji_by_id(id).is_some_and(|e| e.deleted)
    })
    .await;
    assert_eq!(
        bob.read(|core| core.custom_emoji_label(id).map(str::to_owned)),
        Some("parrot_party".to_owned())
    );
    assert!(bob.read(|core| core.live_custom_emoji().next().is_none()));
    bob.act(Input::ToggleReaction {
        conversation: general,
        message,
        emoji: ReactionEmoji::custom(id),
    });
    bob.wait_until("the reaction to be withdrawn", move |core| {
        core.timeline(general).iter().all(|i| {
            i.message_id() != Some(message)
                || i.reactions
                    .iter()
                    .all(|r| r.emoji != ReactionEmoji::custom(id))
        })
    })
    .await;
    // A new message with the old name stays text.
    alice.send("gone: :parrot_party:");
    bob.wait_for_text("gone: :parrot_party:").await;
    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM custom_emoji WHERE deleted_at IS NOT NULL")
            .fetch_one(&world.pool)
            .await
            .unwrap();
    assert_eq!(stored, 1);

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn names_are_unique_never_standard_shortcodes_and_images_must_be_images(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let alice = start(&world, dir.path(), "alice");
    alice.register("alice").await;

    add(
        &alice,
        "dancing_cat",
        file(dir.path(), "cat.png", &png(64, 64)),
    )
    .await;

    // A second emoji with a live name, and one named like a standard shortcode, are refused.
    for (name, label) in [
        ("dancing_cat", "a live name"),
        ("tada", "a standard shortcode"),
    ] {
        alice.act(Input::DismissAction(ActionKind::AddCustomEmoji));
        alice.act(Input::AddCustomEmoji {
            name: name.to_owned(),
            file: file(dir.path(), "again.png", &png(16, 16)),
        });
        alice
            .wait_until(&format!("a refusal for {label}"), |core| {
                matches!(
                    core.action(ActionKind::AddCustomEmoji),
                    ActionState::Failed(_)
                )
            })
            .await;
    }
    // Something that is not an image is refused by the server, whatever it is called.
    alice.act(Input::DismissAction(ActionKind::AddCustomEmoji));
    alice.act(Input::AddCustomEmoji {
        name: "not_a_picture".to_owned(),
        file: file(dir.path(), "fake.png", b"this is plain text, not a picture"),
    });
    alice
        .wait_until("a refusal for a fake picture", |core| {
            matches!(
                core.action(ActionKind::AddCustomEmoji),
                ActionState::Failed(_)
            )
        })
        .await;
    let live: i64 =
        sqlx::query_scalar("SELECT count(*) FROM custom_emoji WHERE deleted_at IS NULL")
            .fetch_one(&world.pool)
            .await
            .unwrap();
    assert_eq!(live, 1, "only the good one was stored");

    // After deleting, the name is free again.
    let id = alice.read(|core| live_named(core, "dancing_cat")).unwrap();
    alice.act(Input::DeleteCustomEmoji(id));
    alice
        .wait_until("the deletion", move |core| {
            core.custom_emoji_by_id(id).is_some_and(|e| e.deleted)
        })
        .await;
    alice.act(Input::DismissAction(ActionKind::AddCustomEmoji));
    add(
        &alice,
        "dancing_cat",
        file(dir.path(), "cat2.png", &png(48, 48)),
    )
    .await;

    alice.stop().await;
    world.server.shutdown().await.unwrap();
}
