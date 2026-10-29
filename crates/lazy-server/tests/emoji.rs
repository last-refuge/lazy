//! Custom emoji: adding, renaming, deleting, serving, and using them in messages and reactions.

mod common;

use std::time::Duration;

use common::*;
use futures_util::future::join_all;
use lazy_domain::{EmojiId, MessageDocument, OperationId, WorkspaceId};
use lazy_protocol::{
    CustomEmoji, ErrorCode, Event, EventEnvelope, Message, PROTOCOL_VERSION, SendMessageResponse,
};
use lazy_server::AttachmentOptions;
use lazy_server::attachments::cleanup;
use lazy_server::attachments::store::{AttachmentStore, FileStore, ObjectKey};
use serde_json::json;
use sqlx::PgPool;

async fn denied(response: reqwest::Response, code: ErrorCode) {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
}

/// An owner and an admin, and three ordinary members, all in #general.
struct Chat {
    app: App,
    owner: Account,
    boss: Account,
    alice: Account,
    bob: Account,
    carol: Account,
}

async fn chat_with(app: App) -> Chat {
    let owner = app.register("owner").await;
    let boss = app.register("boss").await;
    app.set_role(&boss, "admin").await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let carol = app.register("carol").await;
    Chat {
        app,
        owner,
        boss,
        alice,
        bob,
        carol,
    }
}

async fn chat(pool: PgPool) -> Chat {
    chat_with(App::start(pool).await).await
}

async fn finish(chat: Chat) {
    chat.app.server.shutdown().await.unwrap();
}

fn ws(app: &App) -> WorkspaceId {
    app.workspace_id
}

/// A message document with some words and then the given emoji.
fn doc(text: &str, emoji: &[&CustomEmoji]) -> MessageDocument {
    let mut inlines = vec![json!({"type": "text", "text": text})];
    for emoji in emoji {
        inlines.push(json!({"type": "emoji", "id": emoji.id, "name": emoji.name}));
    }
    serde_json::from_value(json!({
        "version": 4,
        "blocks": [{"type": "paragraph", "inlines": inlines}],
    }))
    .expect("a document with emoji")
}

async fn send_doc(app: &App, who: &Account, document: MessageDocument) -> reqwest::Response {
    app.send_doc_raw(who, app.general, document).await
}

async fn message_of(response: reqwest::Response) -> Message {
    assert!(response.status().is_success(), "{}", response.status());
    response
        .json::<SendMessageResponse>()
        .await
        .unwrap()
        .message
}

async fn edit_doc(
    app: &App,
    who: &Account,
    message: &Message,
    document: MessageDocument,
) -> reqwest::Response {
    app.call(
        reqwest::Method::PATCH,
        &lazy_protocol::api::paths::message(message.id),
        who,
        Some(json!({
            "operation_id": OperationId::generate(),
            "base_revision": message.revision,
            "document": document,
        })),
    )
    .await
}

async fn png_of(response: reqwest::Response) -> image::DynamicImage {
    assert!(response.status().is_success(), "{}", response.status());
    image::load_from_memory(&response.bytes().await.unwrap()).expect("a png")
}

async fn replay_from(
    app: &App,
    who: &Account,
    cursor: lazy_domain::EventSeq,
) -> Vec<EventEnvelope> {
    let mut socket = app.connect_ws(who).await;
    socket.subscribe(app.workspace_id, cursor).await;
    socket.replay().await.0
}

fn emoji_events(events: &[EventEnvelope]) -> Vec<CustomEmoji> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::EmojiChanged { emoji } => Some(emoji.clone()),
            _ => None,
        })
        .collect()
}

fn stored_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|shard| shard.file_name() != "tmp" && shard.path().is_dir())
        .map(|shard| std::fs::read_dir(shard.path()).unwrap().count())
        .sum()
}

fn file_of(app: &App, id: EmojiId) -> std::path::PathBuf {
    let simple = id.as_uuid().simple().to_string();
    app.server
        .attachments_dir()
        .join(&simple[..2])
        .join(format!("{simple}.emoji"))
}

// --- Adding ------------------------------------------------------------------------------------

#[sqlx::test]
async fn an_image_becomes_a_png_that_fits_upright_and_the_original_is_not_kept(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let cursor = app.head().await;

    // A wide image is scaled down with its aspect; the answer says who and what.
    let source = png_bytes(400, 200, 255);
    let party = app
        .upload_emoji(&c.boss, ws(app), "party", source.clone())
        .await;
    assert_eq!(
        (party.name.as_str(), party.workspace_id, party.created_by),
        ("party", ws(app), c.boss.user.id)
    );
    assert!(!party.deleted);
    let image = png_of(
        app.emoji_raw(&c.alice, party.id, reqwest::Method::GET, None, &[])
            .await,
    )
    .await;
    assert_eq!((image.width(), image.height()), (128, 64));
    let stored = std::fs::read(file_of(app, party.id)).unwrap();
    assert_ne!(stored, source, "the original bytes are not kept");
    assert_eq!(stored_files(app.server.attachments_dir()), 1);

    // A tall one keeps its aspect; a small one is not enlarged.
    let tall = app
        .upload_emoji(&c.boss, ws(app), "tall_one", png_bytes(100, 300, 255))
        .await;
    let image = png_of(
        app.emoji_raw(&c.alice, tall.id, reqwest::Method::GET, None, &[])
            .await,
    )
    .await;
    assert_eq!(image.height(), 128);
    assert!((42..=43).contains(&image.width()), "{}", image.width());
    let small = app
        .upload_emoji(&c.boss, ws(app), "tiny", png_bytes(40, 30, 255))
        .await;
    let image = png_of(
        app.emoji_raw(&c.alice, small.id, reqwest::Method::GET, None, &[])
            .await,
    )
    .await;
    assert_eq!((image.width(), image.height()), (40, 30));

    // Transparency survives.
    let ghost = app
        .upload_emoji(&c.boss, ws(app), "gloom", png_bytes(60, 60, 100))
        .await;
    let image = png_of(
        app.emoji_raw(&c.alice, ghost.id, reqwest::Method::GET, None, &[])
            .await,
    )
    .await;
    assert_eq!(image.to_rgba8().get_pixel(5, 5).0[3], 100);

    // A GIF is its first frame; a JPEG and a WebP are accepted.
    let mut gif = Vec::new();
    {
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
        for colour in [[230, 20, 20, 255], [20, 20, 230, 255]] {
            encoder
                .encode_frame(image::Frame::new(image::RgbaImage::from_pixel(
                    32,
                    32,
                    image::Rgba(colour),
                )))
                .unwrap();
        }
    }
    let flash = app.upload_emoji(&c.boss, ws(app), "flash", gif).await;
    let image = png_of(
        app.emoji_raw(&c.alice, flash.id, reqwest::Method::GET, None, &[])
            .await,
    )
    .await;
    let pixel = image.to_rgba8().get_pixel(10, 10).0;
    assert!(
        pixel[0] > 200 && pixel[2] < 60,
        "the first frame is red, got {pixel:?}"
    );
    app.upload_emoji(&c.boss, ws(app), "photo", jpeg_with_orientation(120, 80, 1))
        .await;
    let mut webp = Vec::new();
    {
        use image::ImageEncoder;
        let pixels = image::RgbaImage::from_pixel(16, 16, image::Rgba([9, 9, 9, 255]));
        image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
            .write_image(pixels.as_raw(), 16, 16, image::ExtendedColorType::Rgba8)
            .unwrap();
    }
    app.upload_emoji(&c.boss, ws(app), "webby", webp).await;

    // Everyone in the workspace heard of each, in order, as a change to the list.
    let heard: Vec<String> = emoji_events(&replay_from(app, &c.carol, cursor).await)
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(
        heard,
        [
            "party", "tall_one", "tiny", "gloom", "flash", "photo", "webby"
        ]
    );
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM custom_emoji")
            .fetch_one(&app.pool)
            .await
            .unwrap()
            == 7
    );
    finish(c).await;
}

#[sqlx::test]
async fn a_photos_rotation_is_applied_to_emoji_and_to_attachment_thumbnails(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    // 200 by 100 pixels, red on the left, with a note that it should be turned a quarter clockwise.
    let rotated = jpeg_with_orientation(200, 100, 6);
    let emoji = app
        .upload_emoji(&c.boss, ws(app), "turned", rotated.clone())
        .await;
    let image = png_of(
        app.emoji_raw(&c.alice, emoji.id, reqwest::Method::GET, None, &[])
            .await,
    )
    .await;
    // Upright it is 100 by 200, scaled to fit 128: 64 by 128, with the red half now on top.
    assert_eq!((image.width(), image.height()), (64, 128));
    let top = image.to_rgba8().get_pixel(32, 10).0;
    let bottom = image.to_rgba8().get_pixel(32, 118).0;
    assert!(top[0] > 150 && top[2] < 100, "top should be red: {top:?}");
    assert!(
        bottom[2] > 150 && bottom[0] < 100,
        "bottom should be blue: {bottom:?}"
    );
    // The same photo as an attachment: its size and thumbnail are the way up it is shown.
    let attachment = app
        .upload(&c.alice, app.general, rotated, "image/jpeg", "photo.jpg")
        .await;
    assert_eq!(
        (attachment.width, attachment.height),
        (Some(100), Some(200))
    );
    let thumb = app
        .download_raw(&c.alice, attachment.id, true, false, &[])
        .await;
    let thumb = png_or_jpeg_of(thumb).await;
    assert_eq!((thumb.width(), thumb.height()), (100, 200));
    assert!(thumb.to_rgb8().get_pixel(50, 10).0[0] > 150);
    // A photo with no note is left as it is.
    let plain = app
        .upload(
            &c.alice,
            app.general,
            jpeg_with_orientation(200, 100, 1),
            "image/jpeg",
            "p.jpg",
        )
        .await;
    assert_eq!((plain.width, plain.height), (Some(200), Some(100)));
    finish(c).await;
}

async fn png_or_jpeg_of(response: reqwest::Response) -> image::DynamicImage {
    assert!(response.status().is_success());
    image::load_from_memory(&response.bytes().await.unwrap()).expect("an image")
}

#[sqlx::test]
async fn names_follow_the_rules_and_are_unique_among_live_emoji(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let png = png_bytes(20, 20, 255);
    let try_name = |name: Option<&str>| {
        let (app, boss, png) = (&c.app, c.boss.clone(), png.clone());
        let name = name.map(str::to_owned);
        async move {
            app.upload_emoji_raw(&boss, ws(app), EmojiId::generate(), png, name.as_deref())
                .await
        }
    };
    // Not a name, or not allowed to be one (a standard shortcode would be ambiguous).
    for bad in [
        "a",
        &"x".repeat(33),
        "a-b",
        "has space",
        "_ab",
        "ab_",
        "tada",
        "smile",
        "日本語",
    ] {
        denied(try_name(Some(bad)).await, ErrorCode::InvalidRequest).await;
    }
    denied(try_name(None).await, ErrorCode::InvalidRequest).await;
    // Names are lowercase whatever case they arrive in, and the longest and shortest are fine.
    let party = app
        .upload_emoji(&c.boss, ws(app), "PaRtY", png.clone())
        .await;
    assert_eq!(party.name, "party");
    app.upload_emoji(&c.boss, ws(app), "zq", png.clone()).await;
    app.upload_emoji(&c.boss, ws(app), &"z".repeat(32), png.clone())
        .await;
    // A live name is taken, in any case; a deleted one is free; another workspace's does not matter.
    denied(try_name(Some("party")).await, ErrorCode::NameTaken).await;
    denied(try_name(Some("PARTY")).await, ErrorCode::NameTaken).await;
    assert_eq!(
        app.emoji_raw(&c.boss, party.id, reqwest::Method::DELETE, None, &[])
            .await
            .status()
            .as_u16(),
        204
    );
    let again = app
        .upload_emoji(&c.boss, ws(app), "party", png.clone())
        .await;
    assert_ne!(again.id, party.id);
    let elsewhere = app.other_workspace(&[(&c.boss, "admin")]).await;
    app.upload_emoji(&c.boss, elsewhere, "party", png.clone())
        .await;
    // Renaming is held to the same rules.
    denied(
        app.rename_emoji_raw(&c.boss, again.id, "tada").await,
        ErrorCode::InvalidRequest,
    )
    .await;
    denied(
        app.rename_emoji_raw(&c.boss, again.id, "zq").await,
        ErrorCode::NameTaken,
    )
    .await;
    let renamed = app.rename_emoji_raw(&c.boss, again.id, "Fiesta").await;
    assert_eq!(
        renamed
            .json::<lazy_protocol::CustomEmojiResponse>()
            .await
            .unwrap()
            .emoji
            .name,
        "fiesta"
    );
    // Two live emoji never share a name, whatever happened.
    let dupes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (SELECT workspace_id, name FROM custom_emoji WHERE deleted_at IS NULL \
         GROUP BY 1, 2 HAVING count(*) > 1) d",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(dupes, 0);
    finish(c).await;
}

#[sqlx::test]
async fn only_admins_and_owners_manage_emoji_and_only_the_workspace_sees_them(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let png = png_bytes(20, 20, 255);
    let outsider = app.register("outsider").await;
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
        .bind(outsider.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    let upload = |who: &Account, name: &str| {
        let (app, who, png, name) = (&c.app, who.clone(), png.clone(), name.to_owned());
        async move {
            app.upload_emoji_raw(&who, ws(app), EmojiId::generate(), png, Some(&name))
                .await
        }
    };
    // Create: members cannot; admins and owners can; someone outside the workspace is told nothing.
    denied(upload(&c.alice, "mine").await, ErrorCode::Forbidden).await;
    denied(upload(&outsider, "mine").await, ErrorCode::NotFound).await;
    let by_admin = app
        .upload_emoji(&c.boss, ws(app), "from_admin", png.clone())
        .await;
    let by_owner = app
        .upload_emoji(&c.owner, ws(app), "from_owner", png.clone())
        .await;
    assert_eq!(
        (by_admin.created_by, by_owner.created_by),
        (c.boss.user.id, c.owner.user.id)
    );
    // Read: every member; nobody else, and the unknown looks the same as the forbidden.
    for who in [&c.alice, &c.bob, &c.boss] {
        assert_eq!(
            app.emoji_raw(who, by_admin.id, reqwest::Method::GET, None, &[])
                .await
                .status()
                .as_u16(),
            200
        );
    }
    let (status_a, body_a) = api_error(
        app.emoji_raw(&outsider, by_admin.id, reqwest::Method::GET, None, &[])
            .await,
    )
    .await;
    let (status_b, body_b) = api_error(
        app.emoji_raw(
            &outsider,
            EmojiId::generate(),
            reqwest::Method::GET,
            None,
            &[],
        )
        .await,
    )
    .await;
    assert_eq!((status_a, &body_a), (status_b, &body_b));
    let unsigned = app
        .http
        .get(app.url(&lazy_protocol::api::paths::emoji(by_admin.id)))
        .send()
        .await
        .unwrap();
    denied(unsigned, ErrorCode::Unauthenticated).await;
    // Rename: the same people.
    denied(
        app.rename_emoji_raw(&c.alice, by_admin.id, "nope").await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.rename_emoji_raw(&outsider, by_admin.id, "nope").await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.rename_emoji_raw(&c.boss, EmojiId::generate(), "nope")
            .await,
        ErrorCode::NotFound,
    )
    .await;
    assert!(
        app.rename_emoji_raw(&c.owner, by_admin.id, "renamed")
            .await
            .status()
            .is_success()
    );
    // Delete: the same people; a second delete, and one of nothing, are quiet.
    denied(
        app.emoji_raw(&c.alice, by_owner.id, reqwest::Method::DELETE, None, &[])
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.emoji_raw(&outsider, by_owner.id, reqwest::Method::DELETE, None, &[])
            .await,
        ErrorCode::NotFound,
    )
    .await;
    assert_eq!(
        app.emoji_raw(&c.boss, by_owner.id, reqwest::Method::DELETE, None, &[])
            .await
            .status()
            .as_u16(),
        204
    );
    assert_eq!(
        app.emoji_raw(&c.boss, by_owner.id, reqwest::Method::DELETE, None, &[])
            .await
            .status()
            .as_u16(),
        204
    );
    assert_eq!(
        app.emoji_raw(
            &c.boss,
            EmojiId::generate(),
            reqwest::Method::DELETE,
            None,
            &[]
        )
        .await
        .status()
        .as_u16(),
        204
    );
    // A deleted one cannot be fetched or renamed, even by an admin.
    denied(
        app.emoji_raw(&c.alice, by_owner.id, reqwest::Method::GET, None, &[])
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.rename_emoji_raw(&c.boss, by_owner.id, "again").await,
        ErrorCode::NotFound,
    )
    .await;
    // Another workspace's emoji is not ours to manage, even as an admin elsewhere.
    let elsewhere = app
        .other_workspace(&[(&c.boss, "admin"), (&c.bob, "member")])
        .await;
    let foreign = app
        .upload_emoji(&c.boss, elsewhere, "foreign", png.clone())
        .await;
    denied(
        app.emoji_raw(&c.alice, foreign.id, reqwest::Method::GET, None, &[])
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.rename_emoji_raw(&c.alice, foreign.id, "mine").await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.emoji_raw(&c.alice, foreign.id, reqwest::Method::DELETE, None, &[])
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.emoji_raw(&c.bob, foreign.id, reqwest::Method::DELETE, None, &[])
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    // A person removed from the workspace sees nothing more.
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1 AND workspace_id = $2")
        .bind(c.carol.user.id.as_uuid())
        .bind(app.workspace_id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    denied(
        app.emoji_raw(&c.carol, by_admin.id, reqwest::Method::GET, None, &[])
            .await,
        ErrorCode::NotFound,
    )
    .await;
    finish(c).await;
}

#[sqlx::test]
async fn what_may_be_uploaded_is_limited_in_size_pixels_type_and_number(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let put = |bytes: Vec<u8>, name: &str| {
        let (app, boss, name) = (&c.app, c.boss.clone(), name.to_owned());
        async move {
            app.upload_emoji_raw(&boss, ws(app), EmojiId::generate(), bytes, Some(&name))
                .await
        }
    };
    // Too big, before the body is read; a body of the largest size is let through.
    let big = vec![7u8; lazy_domain::emoji::MAX_EMOJI_BYTES as usize + 1];
    let response = put(big, "big").await;
    assert_eq!(response.status().as_u16(), 413);
    denied(response, ErrorCode::PayloadTooLarge).await;
    // Too many pixels (a header the decoder believes), exactly at the limit is fine.
    denied(
        put(png_bytes(2100, 2100, 255), "kaboom").await,
        ErrorCode::UnsupportedMedia,
    )
    .await;
    let widest = put(png_bytes(2048, 2048, 255), "widest").await;
    assert_eq!(widest.status().as_u16(), 200);
    // Not an image the server can decode, or not one of the four types.
    for (what, bytes) in [
        ("garbage png", b"\x89PNG\r\n\x1a\nnonsense".to_vec()),
        ("a zip", b"PK\x03\x04 and more".to_vec()),
        ("a pdf", b"%PDF-1.7 hello".to_vec()),
        ("svg", b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec()),
        ("text", b"just words".to_vec()),
    ] {
        let response = put(bytes, "oops").await;
        assert_eq!(response.status().as_u16(), 415, "{what}");
        denied(response, ErrorCode::UnsupportedMedia).await;
    }
    // Nothing of the refusals was kept.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM custom_emoji")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!((rows, stored_files(app.server.attachments_dir())), (1, 1));
    assert!(
        std::fs::read_dir(app.server.attachments_dir().join("tmp"))
            .unwrap()
            .next()
            .is_none()
    );

    // 500 live is the most; deleting one makes room.
    sqlx::query(
        "INSERT INTO custom_emoji (id, workspace_id, name, created_by, size, sha256, width, height) \
         SELECT gen_random_uuid(), $1, 'filler_' || n, $2, 10, decode(repeat('ab', 32), 'hex'), 8, 8 \
         FROM generate_series(1, 499) n",
    )
    .bind(app.workspace_id.as_uuid())
    .bind(c.boss.user.id.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();
    let response = put(png_bytes(20, 20, 255), "the_501st").await;
    assert_eq!(response.status().as_u16(), 422);
    denied(response, ErrorCode::LimitReached).await;
    sqlx::query("UPDATE custom_emoji SET deleted_at = now() WHERE name = 'filler_1'")
        .execute(&app.pool)
        .await
        .unwrap();
    assert_eq!(
        put(png_bytes(20, 20, 255), "the_501st")
            .await
            .status()
            .as_u16(),
        200
    );
    finish(c).await;
}

#[sqlx::test]
async fn the_same_upload_twice_is_one_emoji_and_the_same_id_for_anything_else_is_a_conflict(
    pool: PgPool,
) {
    let c = chat(pool).await;
    let app = &c.app;
    let png = png_bytes(300, 300, 255);
    let id = EmojiId::generate();
    let first = app
        .upload_emoji_raw(&c.boss, ws(app), id, png.clone(), Some("echoes"))
        .await;
    let first = first
        .json::<lazy_protocol::CustomEmojiResponse>()
        .await
        .unwrap()
        .emoji;
    let head = app.head().await;
    let again = app
        .upload_emoji_raw(&c.boss, ws(app), id, png.clone(), Some("echoes"))
        .await;
    assert_eq!(again.status().as_u16(), 200);
    assert_eq!(
        again
            .json::<lazy_protocol::CustomEmojiResponse>()
            .await
            .unwrap()
            .emoji,
        first
    );
    assert_eq!(app.head().await, head, "a repeat announces nothing");
    assert_eq!(stored_files(app.server.attachments_dir()), 1);
    // The same id with other bytes, another name, another workspace, or after deleting: conflict.
    denied(
        app.upload_emoji_raw(
            &c.boss,
            ws(app),
            id,
            png_bytes(300, 301, 255),
            Some("echoes"),
        )
        .await,
        ErrorCode::Conflict,
    )
    .await;
    denied(
        app.upload_emoji_raw(&c.boss, ws(app), id, png.clone(), Some("renamed"))
            .await,
        ErrorCode::Conflict,
    )
    .await;
    let elsewhere = app.other_workspace(&[(&c.boss, "admin")]).await;
    denied(
        app.upload_emoji_raw(&c.boss, elsewhere, id, png.clone(), Some("echoes"))
            .await,
        ErrorCode::Conflict,
    )
    .await;
    assert_eq!(
        app.emoji_raw(&c.boss, id, reqwest::Method::DELETE, None, &[])
            .await
            .status()
            .as_u16(),
        204
    );
    denied(
        app.upload_emoji_raw(&c.boss, ws(app), id, png.clone(), Some("echoes"))
            .await,
        ErrorCode::Conflict,
    )
    .await;

    // Eight identical uploads at once are one emoji.
    let id = EmojiId::generate();
    let responses = join_all((0..8).map(|_| {
        let (app, boss, png) = (&c.app, c.boss.clone(), png.clone());
        async move {
            app.upload_emoji_raw(&boss, ws(app), id, png, Some("racers"))
                .await
        }
    }))
    .await;
    let mut seen = Vec::new();
    for response in responses {
        assert_eq!(response.status().as_u16(), 200);
        seen.push(
            response
                .json::<lazy_protocol::CustomEmojiResponse>()
                .await
                .unwrap()
                .emoji,
        );
    }
    assert!(seen.iter().all(|e| e == &seen[0]));
    // Eight different emoji wanting one name at once: exactly one gets it.
    let responses = join_all((0..8).map(|_| {
        let (app, boss, png) = (&c.app, c.boss.clone(), png.clone());
        async move {
            app.upload_emoji_raw(&boss, ws(app), EmojiId::generate(), png, Some("contested"))
                .await
        }
    }))
    .await;
    let statuses: Vec<u16> = responses.iter().map(|r| r.status().as_u16()).collect();
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        1,
        "{statuses:?}"
    );
    assert!(statuses.iter().all(|s| *s == 200 || *s == 409));
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM custom_emoji WHERE name = 'contested' AND deleted_at IS NULL",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(live, 1);
    // No stray files or staged uploads from any of that.
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM custom_emoji WHERE image_removed IS NOT TRUE")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(stored_files(app.server.attachments_dir()) as i64, rows);
    assert!(
        std::fs::read_dir(app.server.attachments_dir().join("tmp"))
            .unwrap()
            .next()
            .is_none()
    );
    finish(c).await;
}

// --- Serving -----------------------------------------------------------------------------------

#[sqlx::test]
async fn the_image_is_served_as_an_immutable_png_with_validators(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let party = app
        .upload_emoji(&c.boss, ws(app), "party", png_bytes(64, 64, 255))
        .await;
    let shown = app
        .emoji_raw(&c.bob, party.id, reqwest::Method::GET, None, &[])
        .await;
    let header = |response: &reqwest::Response, name: &str| {
        response
            .headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
    };
    assert_eq!(header(&shown, "content-type").as_deref(), Some("image/png"));
    assert_eq!(
        header(&shown, "cache-control").as_deref(),
        Some("private, max-age=31536000, immutable")
    );
    assert_eq!(
        header(&shown, "x-content-type-options").as_deref(),
        Some("nosniff")
    );
    assert_eq!(
        header(&shown, "content-security-policy").as_deref(),
        Some("default-src 'none'; sandbox")
    );
    let etag = header(&shown, "etag").expect("an etag");
    assert!(etag.starts_with('"') && etag.len() == 66);
    let length: usize = header(&shown, "content-length").unwrap().parse().unwrap();
    assert_eq!(shown.bytes().await.unwrap().len(), length);
    // A matching validator is a 304 without a body; a stale one is a 200; HEAD has no body.
    let unchanged = app
        .emoji_raw(
            &c.bob,
            party.id,
            reqwest::Method::GET,
            None,
            &[("if-none-match", &etag)],
        )
        .await;
    assert_eq!(unchanged.status().as_u16(), 304);
    assert!(unchanged.bytes().await.unwrap().is_empty());
    assert_eq!(
        app.emoji_raw(
            &c.bob,
            party.id,
            reqwest::Method::GET,
            None,
            &[("if-none-match", "\"x\"")]
        )
        .await
        .status()
        .as_u16(),
        200
    );
    let head = app
        .emoji_raw(&c.bob, party.id, reqwest::Method::HEAD, None, &[])
        .await;
    assert_eq!(head.status().as_u16(), 200);
    assert_eq!(header(&head, "etag").as_deref(), Some(etag.as_str()));
    // A deleted emoji answers like one that never existed, validator or not.
    app.emoji_raw(&c.boss, party.id, reqwest::Method::DELETE, None, &[])
        .await;
    denied(
        app.emoji_raw(
            &c.bob,
            party.id,
            reqwest::Method::GET,
            None,
            &[("if-none-match", &etag)],
        )
        .await,
        ErrorCode::NotFound,
    )
    .await;
    finish(c).await;
}

// --- Renaming and deleting ---------------------------------------------------------------------

#[sqlx::test]
async fn renaming_and_deleting_are_announced_and_change_nothing_a_message_already_says(
    pool: PgPool,
) {
    let c = chat(pool).await;
    let app = &c.app;
    let store = FileStore::open_dir(app.server.attachments_dir()).unwrap();
    let cursor = app.head().await;
    let party = app
        .upload_emoji(&c.boss, ws(app), "party", png_bytes(50, 50, 255))
        .await;
    let message = message_of(send_doc(app, &c.alice, doc("hi ", &[&party])).await).await;
    assert_eq!(message.document.custom_emoji(), vec![party.id]);
    assert_eq!(message.document.plain_text(), "hi :party:");
    app.react(&c.bob, message.id, &format!("custom:{}", party.id), true)
        .await;

    // A rename is one event, a repeat of it none, and the message names the emoji by identity.
    let renamed = app.rename_emoji_raw(&c.boss, party.id, "fiesta").await;
    let renamed = renamed
        .json::<lazy_protocol::CustomEmojiResponse>()
        .await
        .unwrap()
        .emoji;
    assert_eq!((renamed.id, renamed.name.as_str()), (party.id, "fiesta"));
    let head = app.head().await;
    assert!(
        app.rename_emoji_raw(&c.boss, party.id, "fiesta")
            .await
            .status()
            .is_success()
    );
    assert_eq!(app.head().await, head);
    assert_eq!(
        app.get_message(&c.alice, message.id).await.document,
        message.document
    );

    // A delete is one event, soft: the list keeps it, marked, its image goes with the next cleanup,
    // its name is free, and what already used it is unchanged.
    assert_eq!(
        app.emoji_raw(&c.boss, party.id, reqwest::Method::DELETE, None, &[])
            .await
            .status()
            .as_u16(),
        204
    );
    let head = app.head().await;
    assert_eq!(
        app.emoji_raw(&c.boss, party.id, reqwest::Method::DELETE, None, &[])
            .await
            .status()
            .as_u16(),
        204
    );
    assert_eq!(app.head().await, head, "deleting again announces nothing");
    assert!(
        file_of(app, party.id).is_file(),
        "the image waits for the job"
    );
    let report = cleanup(&app.pool, &store, Duration::from_secs(24 * 3600))
        .await
        .unwrap();
    assert_eq!(report.emoji_images, 1);
    assert!(!file_of(app, party.id).exists());
    let again = cleanup(&app.pool, &store, Duration::from_secs(24 * 3600))
        .await
        .unwrap();
    assert_eq!(again, Default::default());
    app.upload_emoji(&c.boss, ws(app), "fiesta", png_bytes(50, 50, 255))
        .await;
    let page = app.history_ok(&c.carol, app.general, &[]).await;
    assert_eq!(page.messages[0].document.custom_emoji(), vec![party.id]);
    assert_eq!(
        page.messages[0].reactions[0].emoji.custom_id(),
        Some(party.id)
    );

    // The log tells it: made, renamed, deleted (and the name taken again).
    let events = emoji_events(&replay_from(app, &c.carol, cursor).await);
    let shape: Vec<(String, bool)> = events.iter().map(|e| (e.name.clone(), e.deleted)).collect();
    assert_eq!(
        shape,
        [
            ("party".to_owned(), false),
            ("fiesta".to_owned(), false),
            ("fiesta".to_owned(), true),
            ("fiesta".to_owned(), false)
        ]
    );
    assert!(events[..3].iter().all(|e| e.id == party.id));
    finish(c).await;
}

#[sqlx::test]
async fn the_bootstrap_lists_live_and_deleted_emoji_by_name_and_other_workspaces_hear_nothing(
    pool: PgPool,
) {
    let c = chat(pool).await;
    let app = &c.app;
    let png = png_bytes(30, 30, 255);
    let zinc = app
        .upload_emoji(&c.boss, ws(app), "zinc", png.clone())
        .await;
    let amber = app
        .upload_emoji(&c.boss, ws(app), "amber", png.clone())
        .await;
    let gone = app
        .upload_emoji(&c.boss, ws(app), "marble", png.clone())
        .await;
    app.emoji_raw(&c.boss, gone.id, reqwest::Method::DELETE, None, &[])
        .await;
    let boot = app.bootstrap_ok(&c.carol).await;
    let listed: Vec<(&str, bool)> = boot.workspaces[0]
        .custom_emoji
        .iter()
        .map(|e| (e.name.as_str(), e.deleted))
        .collect();
    assert_eq!(
        listed,
        [("amber", false), ("marble", true), ("zinc", false)]
    );
    assert_eq!(boot.workspaces[0].custom_emoji[0].id, amber.id);
    assert_eq!(boot.workspaces[0].custom_emoji[2].id, zinc.id);
    // Same name, deleted and live: both are listed, deleted first by id order within the name.
    let reuse = app
        .upload_emoji(&c.boss, ws(app), "marble", png.clone())
        .await;
    let boot = app.bootstrap_ok(&c.carol).await;
    let mangos: Vec<(EmojiId, bool)> = boot.workspaces[0]
        .custom_emoji
        .iter()
        .filter(|e| e.name == "marble")
        .map(|e| (e.id, e.deleted))
        .collect();
    let mut expected = vec![(gone.id, true), (reuse.id, false)];
    expected.sort();
    assert_eq!(mangos, expected);

    // A live subscriber hears of each change as it happens; someone in another workspace, nothing.
    let elsewhere = app
        .other_workspace(&[(&c.boss, "admin"), (&c.bob, "member")])
        .await;
    let mut here = app.connect_ws(&c.alice).await;
    here.subscribe(app.workspace_id, app.head().await).await;
    here.replay().await;
    let mut there = app.connect_ws(&c.bob).await;
    there
        .subscribe(elsewhere, lazy_domain::EventSeq::ZERO)
        .await;
    let (seen_there, _) = there.replay().await;
    assert!(emoji_events(&seen_there).is_empty());
    let fresh = app
        .upload_emoji(&c.boss, ws(app), "fresh", png.clone())
        .await;
    let marker = app.register("marker").await;
    let events = here.events_until_marker(marker.user.id).await;
    assert_eq!(emoji_events(&events), vec![fresh]);
    finish(c).await;
}

#[sqlx::test]
async fn a_failure_between_the_image_and_its_row_leaves_nothing_the_sweep_cannot_account_for(
    pool: PgPool,
) {
    let app = App::start_with_attachments(
        pool,
        AttachmentOptions {
            fail_between_file_and_row: true,
            ..AttachmentOptions::default()
        },
    )
    .await;
    let c = chat_with(app).await;
    let app = &c.app;
    let store = FileStore::open_dir(app.server.attachments_dir()).unwrap();
    let response = app
        .upload_emoji_raw(
            &c.boss,
            ws(app),
            EmojiId::generate(),
            png_bytes(30, 30, 255),
            Some("lost"),
        )
        .await;
    assert_eq!(response.status().as_u16(), 500);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM custom_emoji")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(
        (rows, stored_files(app.server.attachments_dir())),
        (0, 1),
        "a file with no row, never a row with no file"
    );
    let report = lazy_server::attachments::sweep_orphans(&app.pool, &store, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(report.orphans, 1);
    assert_eq!(stored_files(app.server.attachments_dir()), 0);
    finish(c).await;
}

#[sqlx::test]
async fn the_sweep_keeps_the_images_of_live_emoji_and_of_deleted_ones_awaiting_cleanup(
    pool: PgPool,
) {
    let c = chat(pool).await;
    let app = &c.app;
    let store = FileStore::open_dir(app.server.attachments_dir()).unwrap();
    let live = app
        .upload_emoji(&c.boss, ws(app), "live_one", png_bytes(30, 30, 255))
        .await;
    let doomed = app
        .upload_emoji(&c.boss, ws(app), "doomed", png_bytes(30, 30, 255))
        .await;
    app.emoji_raw(&c.boss, doomed.id, reqwest::Method::DELETE, None, &[])
        .await;
    let report = lazy_server::attachments::sweep_orphans(&app.pool, &store, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(report.orphans, 0);
    assert!(file_of(app, live.id).is_file() && file_of(app, doomed.id).is_file());
    // Once the job has removed the deleted one, a file that came back would be an orphan.
    cleanup(&app.pool, &store, Duration::from_secs(3600))
        .await
        .unwrap();
    assert!(file_of(app, live.id).is_file() && !file_of(app, doomed.id).exists());
    let stray = ObjectKey::emoji(doomed.id.as_uuid());
    store.put_small(stray, b"stray".to_vec()).await.unwrap();
    let report = lazy_server::attachments::sweep_orphans(&app.pool, &store, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(
        report.orphans, 1,
        "a removed image that reappears is an orphan"
    );
    assert!(file_of(app, live.id).is_file() && !file_of(app, doomed.id).exists());
    finish(c).await;
}

/// Ids are chosen by clients, so an emoji can be given the id of a file (which anyone who can read
/// the message sees) and a file the id of an emoji (which anyone in the workspace sees). They are
/// different objects: neither replaces, reads, or accounts for the other.
#[sqlx::test]
async fn an_emoji_and_a_file_with_one_id_are_different_objects(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let store = FileStore::open_dir(app.server.attachments_dir()).unwrap();
    let bytes_of = |response: reqwest::Response| async move {
        assert!(response.status().is_success(), "{}", response.status());
        response.bytes().await.unwrap().to_vec()
    };

    // A file first, then an emoji with its id: both read as themselves.
    let note = b"the file's own bytes".to_vec();
    let file = app
        .upload(
            &c.alice,
            app.general,
            note.clone(),
            "application/zip",
            "a.bin",
        )
        .await;
    let twin = app
        .upload_emoji_raw(
            &c.boss,
            ws(app),
            EmojiId::from_uuid(file.id.as_uuid()),
            png_bytes(30, 30, 255),
            Some("twin_one"),
        )
        .await;
    assert_eq!(twin.status().as_u16(), 200);
    let twin = twin
        .json::<lazy_protocol::CustomEmojiResponse>()
        .await
        .unwrap()
        .emoji;
    assert_eq!(
        bytes_of(app.download_raw(&c.alice, file.id, false, false, &[]).await).await,
        note
    );
    let image = png_of(
        app.emoji_raw(&c.bob, twin.id, reqwest::Method::GET, None, &[])
            .await,
    )
    .await;
    assert_eq!((image.width(), image.height()), (30, 30));

    // An emoji first, then a file with its id (anyone who can post can try).
    let emoji = app
        .upload_emoji(&c.boss, ws(app), "twin_two", png_bytes(50, 40, 255))
        .await;
    let other = b"another file".to_vec();
    let sent = app
        .upload_raw(
            &c.bob,
            app.general,
            lazy_domain::AttachmentId::from_uuid(emoji.id.as_uuid()),
            other.clone(),
            "application/zip",
            "b.bin",
        )
        .await;
    assert_eq!(sent.status().as_u16(), 200);
    let image = png_of(
        app.emoji_raw(&c.bob, emoji.id, reqwest::Method::GET, None, &[])
            .await,
    )
    .await;
    assert_eq!(
        (image.width(), image.height()),
        (50, 40),
        "the emoji is not replaced by a file"
    );
    let back = app
        .download_raw(
            &c.bob,
            lazy_domain::AttachmentId::from_uuid(emoji.id.as_uuid()),
            false,
            false,
            &[],
        )
        .await;
    assert_eq!(bytes_of(back).await, other);

    // The sweep accounts for each by its own row, and for a file only by a row of its own kind.
    let sweep = || lazy_server::attachments::sweep_orphans(&app.pool, &store, Duration::ZERO);
    assert_eq!(sweep().await.unwrap().orphans, 0);
    let lonely_file = app
        .upload(
            &c.alice,
            app.general,
            b"no emoji has this id".to_vec(),
            "application/zip",
            "c.bin",
        )
        .await;
    let lonely_emoji = app
        .upload_emoji(&c.boss, ws(app), "twin_three", png_bytes(20, 20, 255))
        .await;
    store
        .put_small(
            ObjectKey::original(lonely_emoji.id.as_uuid()),
            b"stray".to_vec(),
        )
        .await
        .unwrap();
    store
        .put_small(
            ObjectKey::emoji(lonely_file.id.as_uuid()),
            b"stray".to_vec(),
        )
        .await
        .unwrap();
    assert_eq!(
        sweep().await.unwrap().orphans,
        2,
        "an emoji row does not account for a file"
    );
    assert!(file_of(app, lonely_emoji.id).is_file());
    assert_eq!(
        bytes_of(
            app.download_raw(&c.alice, lonely_file.id, false, false, &[])
                .await
        )
        .await,
        b"no emoji has this id"
    );
    finish(c).await;
}

// --- Using emoji -------------------------------------------------------------------------------

#[sqlx::test]
async fn a_message_may_use_only_live_emoji_of_its_workspace_but_keeps_what_it_had(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let png = png_bytes(30, 30, 255);
    let live = app
        .upload_emoji(&c.boss, ws(app), "live_one", png.clone())
        .await;
    let old = app
        .upload_emoji(&c.boss, ws(app), "old_one", png.clone())
        .await;
    let elsewhere = app.other_workspace(&[(&c.boss, "admin")]).await;
    let foreign = app
        .upload_emoji(&c.boss, elsewhere, "foreign", png.clone())
        .await;
    let unknown = CustomEmoji {
        id: EmojiId::generate(),
        ..live.clone()
    };

    // Live ones are fine, in any number and place.
    let sent = message_of(send_doc(app, &c.alice, doc("both ", &[&live, &old, &live])).await).await;
    assert_eq!(
        sent.document.plain_text(),
        "both :live_one::old_one::live_one:"
    );
    // Unknown, deleted, and another workspace's are refused, and nothing is sent.
    app.emoji_raw(&c.boss, old.id, reqwest::Method::DELETE, None, &[])
        .await;
    let head = app.head().await;
    for dead in [&unknown, &old, &foreign] {
        denied(
            send_doc(app, &c.alice, doc("no ", &[dead])).await,
            ErrorCode::InvalidRequest,
        )
        .await;
        denied(
            send_doc(app, &c.alice, doc("one good one bad ", &[&live, dead])).await,
            ErrorCode::InvalidRequest,
        )
        .await;
    }
    assert_eq!(app.head().await, head);

    // An edit may keep an emoji the message already had even if it has since been deleted: only the
    // emoji an edit adds are checked. It cannot add a dead one, and a live one is fine.
    let kept = edit_doc(
        app,
        &c.alice,
        &sent,
        doc("still both ", &[&live, &old, &live]),
    )
    .await;
    assert!(kept.status().is_success(), "{}", kept.status());
    let kept = kept
        .json::<lazy_protocol::MessageResponse>()
        .await
        .unwrap()
        .message;
    denied(
        edit_doc(
            app,
            &c.alice,
            &kept,
            doc("now with ", &[&live, &old, &foreign]),
        )
        .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    denied(
        edit_doc(app, &c.alice, &kept, doc("now with ", &[&unknown])).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    let fresh = app
        .upload_emoji(&c.boss, ws(app), "fresh_one", png.clone())
        .await;
    let added = edit_doc(app, &c.alice, &kept, doc("and a new ", &[&old, &fresh])).await;
    assert!(added.status().is_success());

    // Replies are held to the same rule, in the thread and also in the channel.
    let root = app.send(&c.bob, app.general, "a root").await;
    for also in [false, true] {
        denied(
            app.reply_raw(
                &c.alice,
                &root,
                OperationId::generate(),
                doc("r ", &[&old]),
                also,
            )
            .await,
            ErrorCode::InvalidRequest,
        )
        .await;
        let ok = app
            .reply_raw(
                &c.alice,
                &root,
                OperationId::generate(),
                doc("r ", &[&live]),
                also,
            )
            .await;
        assert_eq!(message_of(ok).await.document.custom_emoji(), vec![live.id]);
    }

    // A retry of a send returns the same message, with its emoji, even after the emoji was deleted.
    let operation = OperationId::generate();
    let body = |op| json!({"operation_id": op, "document": doc("retry ", &[&live])});
    let url = lazy_protocol::api::paths::messages(app.general);
    let first = message_of(
        app.call(reqwest::Method::POST, &url, &c.alice, Some(body(operation)))
            .await,
    )
    .await;
    app.emoji_raw(&c.boss, live.id, reqwest::Method::DELETE, None, &[])
        .await;
    let second = message_of(
        app.call(reqwest::Method::POST, &url, &c.alice, Some(body(operation)))
            .await,
    )
    .await;
    assert_eq!(first, second);
    denied(
        app.call(
            reqwest::Method::POST,
            &url,
            &c.alice,
            Some(body(OperationId::generate())),
        )
        .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    finish(c).await;
}

#[sqlx::test]
async fn a_webhook_cannot_use_emoji_and_its_colons_stay_text(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    app.upload_emoji(&c.boss, ws(app), "party", png_bytes(30, 30, 255))
        .await;
    let hook = app.create_webhook(&c.boss, app.general, "Deploys").await;
    let id = app.deliver(&hook, "shipped :party: :tada:").await;
    let message = app.get_message(&c.alice, id).await;
    assert!(message.document.custom_emoji().is_empty());
    let text = message.document.plain_text();
    assert!(text.contains(":party:") && text.contains('🎉'), "{text}");
    finish(c).await;
}

#[sqlx::test]
async fn custom_emoji_are_reactions_by_identity_that_can_always_be_withdrawn(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let png = png_bytes(30, 30, 255);
    let party = app
        .upload_emoji(&c.boss, ws(app), "party", png.clone())
        .await;
    let spare = app
        .upload_emoji(&c.boss, ws(app), "spare", png.clone())
        .await;
    let elsewhere = app.other_workspace(&[(&c.boss, "admin")]).await;
    let foreign = app
        .upload_emoji(&c.boss, elsewhere, "foreign", png.clone())
        .await;
    let message = app.send(&c.alice, app.general, "react to me").await;
    let custom = |emoji: &CustomEmoji| format!("custom:{}", emoji.id);

    // Reactions are stored and ordered like any other, by first use; the text is canonical.
    app.react(&c.bob, message.id, "👍", true).await;
    let reacted = app.react(&c.carol, message.id, &custom(&party), true).await;
    app.react(&c.bob, message.id, "❤️", true).await;
    app.react(&c.bob, message.id, &custom(&party), true).await;
    let order: Vec<String> = app
        .get_message(&c.alice, message.id)
        .await
        .reactions
        .iter()
        .map(|r| r.emoji.to_string())
        .collect();
    assert_eq!(order, ["👍".to_owned(), custom(&party), "❤️".to_owned()]);
    assert_eq!(
        reacted
            .reactions
            .iter()
            .find(|r| r.emoji.custom_id() == Some(party.id))
            .unwrap()
            .users,
        vec![c.carol.user.id]
    );
    let stored: Vec<String> = sqlx::query_scalar("SELECT DISTINCT emoji FROM message_reactions WHERE message_id = $1 AND emoji LIKE 'custom:%'")
        .bind(message.id.as_uuid())
        .fetch_all(&app.pool)
        .await
        .unwrap();
    assert_eq!(stored, vec![format!("custom:{}", party.id)]);

    // Unknown ids and another workspace's cannot be used; a bad spelling is not an emoji.
    for dead in [format!("custom:{}", EmojiId::generate()), custom(&foreign)] {
        let response = app.react_raw(&c.bob, message.id, &dead, true).await;
        denied(response, ErrorCode::InvalidRequest).await;
    }
    denied(
        app.react_raw(&c.bob, message.id, "custom:not-an-id", true)
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;

    // Once deleted it cannot be added, but it can be withdrawn, and the summary keeps naming it.
    app.emoji_raw(&c.boss, party.id, reqwest::Method::DELETE, None, &[])
        .await;
    denied(
        app.react_raw(&c.alice, message.id, &custom(&party), true)
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    let page = app.history_ok(&c.alice, app.general, &[]).await;
    assert!(
        page.messages[0]
            .reactions
            .iter()
            .any(|r| r.emoji.custom_id() == Some(party.id))
    );
    let withdrawn = app
        .react(&c.carol, message.id, &custom(&party), false)
        .await;
    assert!(
        withdrawn
            .reactions
            .iter()
            .any(|r| r.emoji.custom_id() == Some(party.id) && r.users == vec![c.bob.user.id])
    );
    let last = app.react(&c.bob, message.id, &custom(&party), false).await;
    assert!(
        last.reactions.iter().all(|r| r.emoji.custom_id().is_none()),
        "an emoji nobody uses is dropped"
    );

    // A custom emoji is one of the fifty kinds like any other.
    let message = app.send(&c.alice, app.general, "fifty").await;
    let glyphs: Vec<&str> = lazy_domain::emoji::groups()
        .flat_map(lazy_domain::emoji::in_group)
        .map(|e| e.as_str())
        .take(60)
        .collect();
    for glyph in &glyphs[..49] {
        app.react(&c.bob, message.id, glyph, true).await;
    }
    app.react(&c.bob, message.id, &custom(&spare), true).await;
    denied(
        app.react_raw(&c.bob, message.id, glyphs[49], true).await,
        ErrorCode::LimitReached,
    )
    .await;
    // Withdrawing the custom one frees its place for a standard one.
    app.react(&c.bob, message.id, &custom(&spare), false).await;
    app.react(&c.bob, message.id, glyphs[49], true).await;
    denied(
        app.react_raw(&c.bob, message.id, &custom(&spare), true)
            .await,
        ErrorCode::LimitReached,
    )
    .await;
    finish(c).await;
}

/// A message or reaction that uses an emoji and a deletion of it take turns: whatever uses it
/// either commits before the deletion or is refused, and nothing is ever committed using an emoji
/// that was deleted before it.
#[sqlx::test]
async fn a_deletion_racing_messages_and_reactions_that_use_the_emoji_never_loses_to_them(
    pool: PgPool,
) {
    let c = chat(pool).await;
    let app = &c.app;
    let png = png_bytes(30, 30, 255);
    let target = app.send(&c.alice, app.general, "react here").await;
    for round in 0..8 {
        let emoji = app
            .upload_emoji(&c.boss, ws(app), &format!("racer_{round}"), png.clone())
            .await;
        let senders = [&c.alice, &c.bob, &c.carol, &c.alice, &c.bob, &c.carol];
        let (deleted, sent, reacted) = tokio::join!(
            async {
                app.emoji_raw(&c.boss, emoji.id, reqwest::Method::DELETE, None, &[])
                    .await
            },
            join_all(senders.iter().map(|who| {
                let (app, who, emoji) = (&c.app, (*who).clone(), emoji.clone());
                async move { send_doc(app, &who, doc("race ", &[&emoji])).await }
            })),
            join_all([&c.bob, &c.carol].iter().map(|who| {
                let (app, who) = (&c.app, (*who).clone());
                let text = format!("custom:{}", emoji.id);
                async move { app.react_raw(&who, target.id, &text, true).await }
            }))
        );
        assert_eq!(deleted.status().as_u16(), 204);
        let mut landed = 0;
        for response in sent {
            match response.status().as_u16() {
                200 => landed += 1,
                400 => {}
                other => panic!("round {round}: unexpected status {other}"),
            }
        }
        let reactions_landed = reacted.iter().filter(|r| r.status().is_success()).count();
        for response in &reacted {
            assert!(
                matches!(response.status().as_u16(), 200 | 400),
                "round {round}"
            );
        }
        // The deletion's place in the log: everything that used the emoji is before it.
        let deleted_at: i64 = sqlx::query_scalar(
            "SELECT seq FROM events WHERE workspace_id = $1 AND kind = 'emoji_changed' \
             AND payload -> 'emoji' ->> 'id' = $2 AND (payload -> 'emoji' ->> 'deleted') = 'true'",
        )
        .bind(app.workspace_id.as_uuid())
        .bind(emoji.id.to_string())
        .fetch_one(&app.pool)
        .await
        .unwrap();
        let used: Vec<i64> =
            sqlx::query_scalar("SELECT seq FROM messages WHERE document::text LIKE $1")
                .bind(format!("%{}%", emoji.id))
                .fetch_all(&app.pool)
                .await
                .unwrap();
        assert_eq!(used.len(), landed, "round {round}");
        assert!(
            used.iter().all(|seq| *seq < deleted_at),
            "round {round}: a message used it after its deletion"
        );
        let reactions: Vec<i64> =
            sqlx::query_scalar("SELECT seq FROM message_reactions WHERE emoji = $1")
                .bind(format!("custom:{}", emoji.id))
                .fetch_all(&app.pool)
                .await
                .unwrap();
        assert_eq!(reactions.len(), reactions_landed, "round {round}");
        assert!(
            reactions.iter().all(|seq| *seq < deleted_at),
            "round {round}: a reaction used it after its deletion"
        );
        assert!(
            app.log_positions_after(lazy_domain::EventSeq::ZERO)
                .await
                .windows(2)
                .all(|w| w[1] == w[0] + 1)
        );
    }
    finish(c).await;
}

// --- Version -----------------------------------------------------------------------------------

#[sqlx::test]
async fn the_server_reports_a_protocol_version_that_has_custom_emoji(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    const { assert!(PROTOCOL_VERSION >= 9) };
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
    finish(c).await;
}

// --- Using an emoji and deleting it take turns --------------------------------------------------

/// Holds the emoji's row the way a deletion in progress does, and shows that a message, an edit, and
/// a reaction that want to use the emoji wait for it (so they see the outcome) instead of reading
/// the row as it was.
#[sqlx::test]
async fn a_use_waits_for_a_deletion_in_progress_and_then_sees_it(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let png = png_bytes(30, 30, 255);
    let emoji = app.upload_emoji(&c.boss, ws(app), "held_one", png).await;
    let target = app
        .send(&c.alice, app.general, "to edit and react to")
        .await;
    let blocked = Duration::from_millis(600);
    let limit = Duration::from_secs(20);
    {
        // A deletion that commits: every use waits, then is refused.
        let mut tx = app.pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM custom_emoji WHERE id = $1 FOR UPDATE")
            .bind(emoji.id.as_uuid())
            .execute(&mut *tx)
            .await
            .unwrap();
        let head = app.head().await;
        let send = send_doc(app, &c.bob, doc("held ", &[&emoji]));
        let edit = edit_doc(app, &c.alice, &target, doc("held ", &[&emoji]));
        let text = format!("custom:{}", emoji.id);
        let react = app.react_raw(&c.carol, target.id, &text, true);
        tokio::pin!(send, edit, react);
        assert!(
            tokio::time::timeout(blocked, &mut send).await.is_err(),
            "the send did not wait"
        );
        assert!(
            tokio::time::timeout(blocked, &mut edit).await.is_err(),
            "the edit did not wait"
        );
        assert!(
            tokio::time::timeout(blocked, &mut react).await.is_err(),
            "the reaction did not wait"
        );
        sqlx::query("UPDATE custom_emoji SET deleted_at = now() WHERE id = $1")
            .bind(emoji.id.as_uuid())
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        denied(
            tokio::time::timeout(limit, send).await.unwrap(),
            ErrorCode::InvalidRequest,
        )
        .await;
        denied(
            tokio::time::timeout(limit, edit).await.unwrap(),
            ErrorCode::InvalidRequest,
        )
        .await;
        denied(
            tokio::time::timeout(limit, react).await.unwrap(),
            ErrorCode::InvalidRequest,
        )
        .await;
        assert_eq!(
            app.head().await,
            head,
            "nothing used the emoji after its deletion"
        );

        // A holder that lets go without deleting: the uses wait, then go through.
        let emoji = app
            .upload_emoji(&c.boss, ws(app), "held_two", png_bytes(30, 30, 255))
            .await;
        let mut tx = app.pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM custom_emoji WHERE id = $1 FOR UPDATE")
            .bind(emoji.id.as_uuid())
            .execute(&mut *tx)
            .await
            .unwrap();
        let send = send_doc(app, &c.bob, doc("held ", &[&emoji]));
        let text = format!("custom:{}", emoji.id);
        let react = app.react_raw(&c.carol, target.id, &text, true);
        tokio::pin!(send, react);
        assert!(
            tokio::time::timeout(blocked, &mut send).await.is_err(),
            "the send did not wait"
        );
        assert!(
            tokio::time::timeout(blocked, &mut react).await.is_err(),
            "the reaction did not wait"
        );
        tx.rollback().await.unwrap();
        assert!(
            tokio::time::timeout(limit, send)
                .await
                .unwrap()
                .status()
                .is_success()
        );
        assert!(
            tokio::time::timeout(limit, react)
                .await
                .unwrap()
                .status()
                .is_success()
        );

        // Uses share the row with each other: they never wait on one another.
        let mut tx = app.pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM custom_emoji WHERE id = $1 FOR SHARE")
            .bind(emoji.id.as_uuid())
            .execute(&mut *tx)
            .await
            .unwrap();
        let sent = tokio::time::timeout(limit, send_doc(app, &c.bob, doc("shared ", &[&emoji])))
            .await
            .expect("a share does not block a share");
        assert!(sent.status().is_success());
        tx.rollback().await.unwrap();
    }
    finish(c).await;
}

// --- Uploads that stop short or promise too much, and races for the limit and for an id -----------

/// A request written by hand to the server, for the things the client library will not do: say
/// nothing of a body's length, say more than it sends, or stop sending.
async fn raw_request(
    addr: std::net::SocketAddr,
    head: &str,
    body: &[u8],
    stop_after_body: bool,
) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(b"\r\n").await.unwrap();
    let _ = stream.write_all(body).await;
    if stop_after_body {
        let _ = stream.shutdown().await;
    }
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut answer)).await;
    let text = String::from_utf8_lossy(&answer).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, text)
}

#[sqlx::test]
async fn a_promise_of_too_much_or_no_promise_is_refused_before_the_body_and_a_short_body_leaves_nothing(
    pool: PgPool,
) {
    let c = chat(pool).await;
    let app = &c.app;
    let head = |extra: &str, name: &str| {
        format!(
            "PUT {} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\n\
             Content-Type: application/octet-stream\r\nx-emoji-name: {name}\r\n\
             Connection: close\r\n{extra}",
            lazy_protocol::api::paths::emoji_upload(ws(app), EmojiId::generate()),
            app.server.addr,
            c.boss.token,
        )
    };
    // A header that promises too much is answered while the client is still silent.
    let promise = format!(
        "Content-Length: {}\r\n",
        lazy_domain::emoji::MAX_EMOJI_BYTES + 1
    );
    let (status, answer) =
        raw_request(app.server.addr, &head(&promise, "too_big"), b"", false).await;
    assert_eq!(status, 413, "{answer}");
    assert!(answer.contains("payload_too_large"));
    // No length at all (a chunked body) is refused too.
    let (status, answer) = raw_request(
        app.server.addr,
        &head("Transfer-Encoding: chunked\r\n", "chunky"),
        b"5\r\nhello\r\n0\r\n\r\n",
        false,
    )
    .await;
    assert_eq!(status, 400, "{answer}");
    assert!(answer.contains("Content-Length is required"));
    // A body that stops short of what the header said is not stored, and leaves nothing behind.
    let png = png_bytes(40, 40, 255);
    let promise = format!("Content-Length: {}\r\n", png.len() + 500);
    let _ = raw_request(app.server.addr, &head(&promise, "stopped"), &png, true).await;
    let tmp = app.server.attachments_dir().join("tmp");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::fs::read_dir(&tmp).is_ok_and(|mut dir| dir.next().is_some()) {
        assert!(
            std::time::Instant::now() < deadline,
            "the staged upload stayed"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM custom_emoji")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!((rows, stored_files(app.server.attachments_dir())), (0, 0));
    finish(c).await;
}

#[sqlx::test]
async fn uploads_racing_for_the_last_places_cannot_pass_the_limit(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    sqlx::query(
        "INSERT INTO custom_emoji (id, workspace_id, name, created_by, size, sha256, width, height) \
         SELECT gen_random_uuid(), $1, 'filler_' || n, $2, 10, decode(repeat('ab', 32), 'hex'), 8, 8 \
         FROM generate_series(1, 497) n",
    )
    .bind(app.workspace_id.as_uuid())
    .bind(c.boss.user.id.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();
    // Three places left and ten uploads wanting them: exactly three win and the rest are told so.
    let png = png_bytes(20, 20, 255);
    let responses = join_all((0..10).map(|n| {
        let (app, boss, png) = (&c.app, c.boss.clone(), png.clone());
        async move {
            app.upload_emoji_raw(
                &boss,
                ws(app),
                EmojiId::generate(),
                png,
                Some(&format!("late_{n}")),
            )
            .await
        }
    }))
    .await;
    let statuses: Vec<u16> = responses.iter().map(|r| r.status().as_u16()).collect();
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        3,
        "{statuses:?}"
    );
    assert!(
        statuses.iter().all(|s| *s == 200 || *s == 422),
        "{statuses:?}"
    );
    let live: i64 =
        sqlx::query_scalar("SELECT count(*) FROM custom_emoji WHERE deleted_at IS NULL")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(live, 500);
    finish(c).await;
}

#[sqlx::test]
async fn one_id_raced_into_two_workspaces_belongs_to_exactly_one(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let elsewhere = app.other_workspace(&[(&c.boss, "admin")]).await;
    let png = png_bytes(20, 20, 255);
    for round in 0..6 {
        let id = EmojiId::generate();
        let (name_a, name_b) = (format!("idrace_{round}a"), format!("idrace_{round}b"));
        let (first, second) = tokio::join!(
            app.upload_emoji_raw(&c.boss, ws(app), id, png.clone(), Some(&name_a)),
            app.upload_emoji_raw(&c.boss, elsewhere, id, png.clone(), Some(&name_b)),
        );
        let mut statuses = [first.status().as_u16(), second.status().as_u16()];
        statuses.sort_unstable();
        assert_eq!(statuses, [200, 409], "round {round}");
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM custom_emoji WHERE id = $1")
            .bind(id.as_uuid())
            .fetch_one(&app.pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }
    finish(c).await;
}

#[sqlx::test]
async fn eight_emoji_renamed_to_one_name_at_once_leave_exactly_one_with_it(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let png = png_bytes(20, 20, 255);
    for round in 0..3 {
        let mut ids = Vec::new();
        for n in 0..8 {
            let name = format!("claimant_{round}_{n}");
            ids.push(
                app.upload_emoji(&c.boss, ws(app), &name, png.clone())
                    .await
                    .id,
            );
        }
        let target = format!("wanted_{round}");
        let responses = join_all(ids.iter().map(|id| {
            let (app, boss, target) = (&c.app, c.boss.clone(), target.clone());
            let id = *id;
            async move { app.rename_emoji_raw(&boss, id, &target).await }
        }))
        .await;
        let statuses: Vec<u16> = responses.iter().map(|r| r.status().as_u16()).collect();
        assert_eq!(
            statuses.iter().filter(|s| **s == 200).count(),
            1,
            "round {round}: {statuses:?}"
        );
        assert!(
            statuses.iter().all(|s| *s == 200 || *s == 409),
            "round {round}: {statuses:?}"
        );
        let holders: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM custom_emoji WHERE name = $1 AND deleted_at IS NULL",
        )
        .bind(&target)
        .fetch_one(&app.pool)
        .await
        .unwrap();
        assert_eq!(holders, 1);
    }
    finish(c).await;
}
