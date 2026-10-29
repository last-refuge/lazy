//! Phase 3A end to end: files are chosen, uploaded, attached to messages, previewed, and
//! downloaded across real clients, a real server (with a real attachments directory), and real
//! Postgres.

mod support;

use std::path::PathBuf;

use lazy_client_core::{
    AttachmentState, ClientCore, ComposerTarget, DownloadState, FileSource, ImageState, Input,
    LocalFile,
};
use lazy_client_runtime::MemoryStorage;
use lazy_domain::{ConversationId, MessageId};
use lazy_protocol::Attachment;
use sha2::{Digest, Sha256};
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

/// A real PNG of a gradient, so the server has an image to decode and shrink.
fn png(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
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

fn write(dir: &std::path::Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn local(path: &std::path::Path, name: &str) -> LocalFile {
    LocalFile {
        source: FileSource::Path(path.to_owned()),
        name: name.to_owned(),
    }
}

async fn attach(client: &Client, target: ComposerTarget, files: Vec<LocalFile>) {
    let count = files.len();
    client.act(Input::AttachFiles { target, files });
    client
        .wait_until("the files to upload", move |core| {
            let chips = core.composer_attachments(target);
            chips.len() == count
                && chips
                    .iter()
                    .all(|c| matches!(c.state, AttachmentState::Uploaded))
        })
        .await;
}

fn message_files(core: &ClientCore, conversation: ConversationId, text: &str) -> Vec<Attachment> {
    core.timeline(conversation)
        .iter()
        .find(|i| i.document.plain_text() == text)
        .map(|i| i.attachments.clone())
        .unwrap_or_default()
}

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn files_are_attached_previewed_and_downloaded_by_someone_else(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);

    let text = b"the quarterly numbers\nare in here\n".to_vec();
    let picture = png(320, 200);
    let notes = write(dir.path(), "notes.txt", &text);
    let target = ComposerTarget::Conversation(general);
    attach(
        &alice,
        target,
        vec![
            local(&notes, "notes.txt"),
            LocalFile {
                source: FileSource::Bytes(lazy_client_core::FileBytes::new(picture.clone())),
                name: "chart.png".to_owned(),
            },
        ],
    )
    .await;
    alice.send("see attached");
    alice.wait_confirmed("see attached").await;
    // Sending empties the composer's tray.
    assert!(alice.read(|core| core.composer_attachments(target).is_empty()));

    // Bob sees both files, as the server decided them.
    bob.wait_for_text("see attached").await;
    let files = bob.read(|core| message_files(core, general, "see attached"));
    assert_eq!(files.len(), 2);
    let (notes_file, chart) = (&files[0], &files[1]);
    assert_eq!(notes_file.name, "notes.txt");
    assert_eq!(notes_file.media_type, "text/plain");
    assert_eq!(notes_file.size, text.len() as u64);
    assert_eq!(notes_file.sha256, sha_hex(&text));
    assert!(!notes_file.has_thumbnail);
    assert_eq!(chart.media_type, "image/png");
    assert_eq!((chart.width, chart.height), (Some(320), Some(200)));
    assert!(chart.has_thumbnail);
    assert_eq!(chart.sha256, sha_hex(&picture));

    // A preview arrives for the image and is smaller than the original.
    bob.act(Input::LoadThumbnail(chart.id));
    let id = chart.id;
    bob.wait_until("the thumbnail", move |core| {
        matches!(core.thumbnail(id), Some(ImageState::Ready(_)))
    })
    .await;
    let Some(ImageState::Ready(thumb)) = bob.read(|core| core.thumbnail(id).cloned()) else {
        unreachable!()
    };
    assert!(!thumb.is_empty());

    // Downloading writes exactly the bytes that were sent, and checks them.
    let saved = dir.path().join("saved-notes.txt");
    bob.act(Input::DownloadAttachment {
        attachment: notes_file.clone(),
        destination: saved.clone(),
    });
    let notes_id = notes_file.id;
    bob.wait_until("the download", move |core| {
        matches!(core.download(notes_id), Some(DownloadState::Done { .. }))
    })
    .await;
    assert_eq!(std::fs::read(&saved).unwrap(), text);
    let full = dir.path().join("saved-chart.png");
    bob.act(Input::DownloadAttachment {
        attachment: chart.clone(),
        destination: full.clone(),
    });
    bob.wait_until("the second download", move |core| {
        matches!(core.download(id), Some(DownloadState::Done { .. }))
    })
    .await;
    assert_eq!(std::fs::read(&full).unwrap(), picture);

    // The bytes are on the server's disk, not in the database.
    let on_disk: usize = walk(world.server.attachments_dir());
    assert!(on_disk >= 2, "{on_disk} files");

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

fn walk(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| {
                    let path = e.path();
                    if path.is_dir() { walk(&path) } else { 1 }
                })
                .sum()
        })
        .unwrap_or(0)
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_message_can_be_only_files_and_what_a_file_claims_to_be_is_not_trusted(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);
    let target = ComposerTarget::Conversation(general);

    // Three files named to look like something they are not.
    let html = write(dir.path(), "page.html", b"<script>alert(1)</script>");
    let fake_png = write(dir.path(), "photo.png", b"this is text, not a picture");
    let svg = write(
        dir.path(),
        "logo.svg",
        b"<svg xmlns='http://www.w3.org/2000/svg'/>",
    );
    attach(
        &alice,
        target,
        vec![
            local(&html, "page.html"),
            local(&fake_png, "photo.png"),
            local(&svg, "logo.svg"),
        ],
    )
    .await;
    // No text at all: the files are the message.
    alice.send("");
    alice
        .wait_until("the files-only message", move |core| {
            core.timeline(general)
                .iter()
                .any(|i| i.attachments.len() == 3 && i.is_confirmed())
        })
        .await;
    bob.wait_until("the message to arrive", move |core| {
        core.timeline(general)
            .iter()
            .any(|i| i.attachments.len() == 3)
    })
    .await;
    let item = bob.read(|core| {
        core.timeline(general)
            .iter()
            .find(|i| i.attachments.len() == 3)
            .cloned()
            .unwrap()
    });
    assert!(item.document.has_no_content());
    assert_eq!(item.document.plain_text(), "");
    for file in &item.attachments {
        assert_eq!(
            file.media_type, "application/octet-stream",
            "{} must not be served as what it claims",
            file.name
        );
        assert!(!file.has_thumbnail, "{}", file.name);
    }
    // It counted as unread for Bob, who is looking elsewhere in the sense that nothing was read.
    assert_eq!(
        alice.read(|core| core.user_state(general).unwrap().unread_count),
        0
    );

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn limits_are_explained_and_a_deleted_message_takes_its_files_with_it(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);
    let target = ComposerTarget::Conversation(general);

    // A file over the limit is refused before anything is uploaded.
    let big = dir.path().join("huge.bin");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(lazy_domain::attachment::MAX_ATTACHMENT_BYTES + 1)
        .unwrap();
    alice.act(Input::AttachFiles {
        target,
        files: vec![local(&big, "huge.bin")],
    });
    alice
        .wait_until("the refusal", move |core| {
            core.composer_attachments(target)
                .iter()
                .any(|c| matches!(c.state, AttachmentState::Failed { .. }))
                || core.notice().is_some()
        })
        .await;
    let uploaded: i64 = sqlx::query_scalar("SELECT count(*) FROM attachments")
        .fetch_one(&world.pool)
        .await
        .unwrap();
    assert_eq!(uploaded, 0, "nothing reached the server");
    // Take it out again.
    let ids: Vec<_> = alice.read(|core| {
        core.composer_attachments(target)
            .iter()
            .map(|c| c.id)
            .collect()
    });
    for attachment in ids {
        alice.act(Input::RemoveAttachment { target, attachment });
    }
    alice.act(Input::DismissNotice);

    // A good file, sent, then the message is deleted: the file is unreachable at once.
    let note = write(dir.path(), "secret.txt", b"for your eyes only");
    attach(&alice, target, vec![local(&note, "secret.txt")]).await;
    alice.send("here it is");
    alice.wait_confirmed("here it is").await;
    bob.wait_for_text("here it is").await;
    let (message, file): (MessageId, Attachment) = bob.read(|core| {
        let item = core
            .timeline(general)
            .iter()
            .find(|i| i.document.plain_text() == "here it is")
            .unwrap();
        (item.message_id().unwrap(), item.attachments[0].clone())
    });
    alice.act(Input::DeleteMessage {
        conversation: general,
        message,
    });
    bob.wait_until("the message to go", move |core| {
        core.timeline(general)
            .iter()
            .all(|i| i.message_id() != Some(message))
    })
    .await;
    let saved = dir.path().join("too-late.txt");
    bob.act(Input::DownloadAttachment {
        attachment: file.clone(),
        destination: saved.clone(),
    });
    let id = file.id;
    bob.wait_until("the download to be refused", move |core| {
        matches!(core.download(id), Some(DownloadState::Failed { .. }))
    })
    .await;
    assert!(!saved.exists(), "no file was written");
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM attachments WHERE deleted_at IS NULL")
        .fetch_one(&world.pool)
        .await
        .unwrap();
    assert_eq!(live, 0);

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_thread_reply_can_carry_files(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);
    alice.send("a thread to reply in");
    alice.wait_confirmed("a thread to reply in").await;
    bob.wait_for_text("a thread to reply in").await;
    let root = alice.read(|core| {
        core.timeline(general)
            .iter()
            .find(|i| i.document.plain_text() == "a thread to reply in")
            .and_then(|i| i.message_id())
            .unwrap()
    });
    alice.act(Input::OpenThread {
        conversation: general,
        root,
    });
    alice
        .wait_until("the pane", move |core| core.open_thread().is_some())
        .await;
    let file = write(dir.path(), "reply.txt", b"details");
    let target = ComposerTarget::Thread(root);
    attach(&alice, target, vec![local(&file, "reply.txt")]).await;
    alice.act(Input::SendReply {
        conversation: general,
        root,
        text: "attached in the thread".to_owned(),
        also_in_channel: false,
    });
    bob.act(Input::OpenThread {
        conversation: general,
        root,
    });
    bob.wait_until("the reply with its file", move |core| {
        core.thread(root).is_some_and(|t| {
            t.replies.iter().any(|r| {
                r.document.plain_text() == "attached in the thread"
                    && r.attachments.len() == 1
                    && r.is_confirmed()
            })
        })
    })
    .await;
    // The channel does not show the reply, and its file stays out of the channel.
    assert!(bob.read(|core| {
        core.timeline(general)
            .iter()
            .all(|i| i.attachments.is_empty())
    }));

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}
