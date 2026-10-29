//! Attachments: uploading, what the server decides a file is, attaching files to messages,
//! downloading them, deleting them, the cleanup jobs, and what survives a crash.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::*;
use futures_util::future::join_all;
use lazy_domain::attachment::MAX_ATTACHMENT_BYTES;
use lazy_domain::{AttachmentId, ConversationId, MessageDocument, OperationId};
use lazy_protocol::{
    AttachmentResponse, ErrorCode, Event, EventEnvelope, Message, PROTOCOL_VERSION,
};
use lazy_server::AttachmentOptions;
use lazy_server::attachments::store::{AttachmentStore, FileStore, ObjectKey};
use lazy_server::attachments::{cleanup, sweep_orphans};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

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

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Bytes that look like nothing in particular and are not text.
fn blob(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

async fn bytes_of(response: reqwest::Response) -> Vec<u8> {
    assert!(response.status().is_success(), "{}", response.status());
    response.bytes().await.unwrap().to_vec()
}

/// Every file the store holds that is committed, by name.
fn stored_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for shard in std::fs::read_dir(dir).unwrap().flatten() {
        if shard.file_name() == "tmp" || !shard.path().is_dir() {
            continue;
        }
        for file in std::fs::read_dir(shard.path()).unwrap().flatten() {
            files.push(file.path());
        }
    }
    files.sort();
    files
}

fn staged_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir.join("tmp"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect()
}

async fn rows(app: &App) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM attachments")
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

/// No row may be without its file (the other way round is for the sweep).
async fn every_row_has_its_file(app: &App) {
    let ids: Vec<(Uuid, bool)> = sqlx::query_as("SELECT id, has_thumbnail FROM attachments")
        .fetch_all(&app.pool)
        .await
        .unwrap();
    let dir = app.server.attachments_dir();
    for (id, thumbnail) in ids {
        let simple = id.simple().to_string();
        assert!(
            dir.join(&simple[..2]).join(&simple).is_file(),
            "row {id} has no file"
        );
        if thumbnail {
            assert!(
                dir.join(&simple[..2])
                    .join(format!("{simple}.thumb"))
                    .is_file(),
                "row {id} has no thumbnail"
            );
        }
    }
}

/// Waits (bounded) for a condition that depends on the server finishing something in the
/// background, such as noticing that a client went away.
async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

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

fn put_head(
    app: &App,
    who: &Account,
    conversation: ConversationId,
    id: AttachmentId,
    extra: &str,
) -> String {
    format!(
        "PUT {} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n{extra}",
        lazy_protocol::api::paths::attachment_upload(conversation, id),
        app.server.addr,
        who.token,
    )
}

fn files_of(message: &Message) -> Vec<AttachmentId> {
    message.attachments.iter().map(|a| a.id).collect()
}

// --- Uploading ---------------------------------------------------------------------------------

#[sqlx::test]
async fn an_upload_round_trips_with_its_hash_and_an_image_gets_dimensions_and_a_thumbnail(
    pool: PgPool,
) {
    let c = chat(pool).await;
    let app = &c.app;
    let picture = png_bytes(1000, 500, 255);
    let id = AttachmentId::generate();
    let response = app
        .upload_raw(
            &c.alice,
            app.general,
            id,
            picture.clone(),
            "image/png",
            "sunset photo.png",
        )
        .await;
    assert_eq!(response.status().as_u16(), 200);
    let attachment = response
        .json::<AttachmentResponse>()
        .await
        .unwrap()
        .attachment;
    assert_eq!(attachment.id, id);
    assert_eq!(attachment.name, "sunset photo.png");
    assert_eq!(attachment.media_type, "image/png");
    assert_eq!(attachment.size, picture.len() as u64);
    assert_eq!(attachment.sha256, sha_hex(&picture));
    assert_eq!(
        (attachment.width, attachment.height),
        (Some(1000), Some(500))
    );
    assert!(attachment.has_thumbnail);

    // The bytes come back exactly (pending: to the uploader), and so does a thumbnail that fits.
    assert_eq!(
        bytes_of(app.download_raw(&c.alice, id, false, false, &[]).await).await,
        picture
    );
    let thumb = app.download_raw(&c.alice, id, true, false, &[]).await;
    assert_eq!(thumb.headers()["content-type"], "image/jpeg");
    let decoded = image::load_from_memory(&bytes_of(thumb).await).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (640, 320));

    // A file that is not an image has neither.
    let document = b"%PDF-1.7 not really a pdf but it starts like one".to_vec();
    let pdf = app
        .upload(
            &c.alice,
            app.general,
            document.clone(),
            "application/pdf",
            "paper.pdf",
        )
        .await;
    assert_eq!(
        (
            pdf.media_type.as_str(),
            pdf.width,
            pdf.height,
            pdf.has_thumbnail
        ),
        ("application/pdf", None, None, false)
    );
    denied(
        app.download_raw(&c.alice, pdf.id, true, false, &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    assert_eq!(
        bytes_of(app.download_raw(&c.alice, pdf.id, false, false, &[]).await).await,
        document
    );

    // Gifs and webps are images too (their first frame is the thumbnail); transparency keeps a png.
    let mut gif = Vec::new();
    image::codecs::gif::GifEncoder::new(&mut gif)
        .encode_frame(image::Frame::new(image::RgbaImage::from_pixel(
            30,
            20,
            image::Rgba([1, 2, 3, 255]),
        )))
        .unwrap();
    let gif = app
        .upload(&c.alice, app.general, gif, "image/gif", "dot.gif")
        .await;
    assert_eq!(
        (
            gif.media_type.as_str(),
            gif.width,
            gif.height,
            gif.has_thumbnail
        ),
        ("image/gif", Some(30), Some(20), true)
    );
    let mut webp = Vec::new();
    {
        use image::ImageEncoder;
        let pixels = image::RgbaImage::from_pixel(16, 16, image::Rgba([9, 9, 9, 255]));
        image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
            .write_image(pixels.as_raw(), 16, 16, image::ExtendedColorType::Rgba8)
            .unwrap();
    }
    let webp = app
        .upload(&c.alice, app.general, webp, "image/webp", "dot.webp")
        .await;
    assert_eq!(
        (webp.media_type.as_str(), webp.has_thumbnail),
        ("image/webp", true)
    );
    let ghost = app
        .upload(
            &c.alice,
            app.general,
            png_bytes(20, 20, 90),
            "image/png",
            "ghost.png",
        )
        .await;
    let thumb = app.download_raw(&c.alice, ghost.id, true, false, &[]).await;
    assert_eq!(thumb.headers()["content-type"], "image/png");
    every_row_has_its_file(app).await;
    assert!(
        staged_files(app.server.attachments_dir()).is_empty(),
        "nothing is left staged"
    );
    finish(c).await;
}

#[sqlx::test]
async fn the_server_decides_what_a_file_is_from_its_first_bytes(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let zip = b"PK\x03\x04 and the rest of an archive".to_vec();
    let cases: Vec<(&str, Vec<u8>, &str, bool)> = vec![
        // What the bytes say beats what the uploader says, both ways.
        (
            "a png that claims to be a zip",
            png_bytes(8, 8, 255),
            "application/zip",
            true,
        ),
        (
            "a zip that claims to be a png",
            zip.clone(),
            "image/png",
            false,
        ),
        (
            "text that claims to be a png",
            b"just words".to_vec(),
            "image/png",
            false,
        ),
        // Active content is never served as itself.
        (
            "html",
            b"<html><script>alert(1)</script></html>".to_vec(),
            "text/html",
            false,
        ),
        (
            "html with parameters",
            b"<html></html>".to_vec(),
            "TEXT/HTML; charset=utf-8",
            false,
        ),
        (
            "svg",
            b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec(),
            "image/svg+xml",
            false,
        ),
        (
            "xml",
            b"<?xml version='1.0'?><a/>".to_vec(),
            "application/xml",
            false,
        ),
        ("a script", b"alert(1)".to_vec(), "text/javascript", false),
        // Plain, well formed types survive.
        ("plain text", b"hello there".to_vec(), "text/plain", false),
        ("json", b"{\"a\":1}".to_vec(), "application/json", false),
        // Nothing to go on.
        ("no type", b"hello".to_vec(), "", false),
        (
            "a nonsense type",
            b"hello".to_vec(),
            "not a media type",
            false,
        ),
    ];
    let expected = [
        "image/png",
        "application/zip",
        "application/octet-stream",
        "application/octet-stream",
        "application/octet-stream",
        "application/octet-stream",
        "application/octet-stream",
        "application/octet-stream",
        "text/plain",
        "application/json",
        "application/octet-stream",
        "application/octet-stream",
    ];
    for ((what, bytes, claimed, _), expected) in cases.into_iter().zip(expected) {
        let attachment = app
            .upload(&c.alice, app.general, bytes, claimed, "file")
            .await;
        assert_eq!(attachment.media_type, expected, "{what}");
    }
    // Only a recognised image is ever shown inline.
    let text = app
        .upload(
            &c.alice,
            app.general,
            b"hello".to_vec(),
            "text/plain",
            "n.txt",
        )
        .await;
    let shown = app.download_raw(&c.alice, text.id, false, false, &[]).await;
    assert!(
        shown.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment;")
    );
    finish(c).await;
}

#[sqlx::test]
async fn names_are_made_safe_and_decoded_from_the_header(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let name_of = |header: Option<&str>| {
        let (app, alice) = (&c.app, c.alice.clone());
        let header = header.map(str::to_owned);
        async move {
            let response = app
                .upload_with_name_header_raw(
                    &alice,
                    app.general,
                    AttachmentId::generate(),
                    blob(10, 1),
                    "application/zip",
                    header.as_deref(),
                )
                .await;
            (response.status().as_u16(), response.text().await.unwrap())
        }
    };
    let accepted = |(status, body): (u16, String)| {
        assert_eq!(status, 200, "{body}");
        serde_json::from_str::<AttachmentResponse>(&body)
            .unwrap()
            .attachment
            .name
    };
    assert_eq!(
        accepted(name_of(Some("report%20final.pdf")).await),
        "report final.pdf"
    );
    assert_eq!(
        accepted(name_of(Some(&percent_encode("日本語 ☕.txt"))).await),
        "日本語 ☕.txt"
    );
    // Paths, controls and disguises are removed.
    assert_eq!(
        accepted(name_of(Some(&percent_encode("../../etc/passwd"))).await),
        "_.._etc_passwd"
    );
    assert_eq!(
        accepted(name_of(Some(&percent_encode("C:\\Users\\a\\b.txt"))).await),
        "C__Users_a_b.txt"
    );
    assert_eq!(
        accepted(name_of(Some(&percent_encode("photo\u{202e}gpj.exe"))).await),
        "photo_gpj.exe"
    );
    assert_eq!(accepted(name_of(Some("a%00b%0Ac.txt")).await), "a_b_c.txt");
    assert_eq!(accepted(name_of(Some(".hidden")).await), "hidden");
    // Nothing, or nothing left, is `file`; a very long name is cut on a character boundary.
    assert_eq!(accepted(name_of(None).await), "file");
    assert_eq!(accepted(name_of(Some("")).await), "file");
    assert_eq!(accepted(name_of(Some("...")).await), "file");
    let long = accepted(name_of(Some(&percent_encode(&"é".repeat(300)))).await);
    assert!(long.len() <= 255 && long.chars().all(|c| c == 'é'));
    // Bad encodings are refused, not guessed at.
    for bad in ["%zz", "%4", "%", "%FF", "%C3"] {
        let (status, body) = name_of(Some(bad)).await;
        assert_eq!(status, 400, "{bad}: {body}");
        assert!(body.contains("invalid_request"));
    }
    let before = rows(app).await;
    assert_eq!(before, 11, "the refused ones left nothing");
    every_row_has_its_file(app).await;
    finish(c).await;
}

#[sqlx::test]
async fn the_size_limit_is_checked_before_the_body_and_again_while_it_streams(pool: PgPool) {
    let app = App::start_with_attachments(
        pool,
        AttachmentOptions {
            max_bytes: 1000,
            ..AttachmentOptions::default()
        },
    )
    .await;
    let c = chat_with(app).await;
    let app = &c.app;
    let dir = app.server.attachments_dir().to_owned();

    // Exactly at the limit is fine; one byte over is refused.
    let exact = app
        .upload(
            &c.alice,
            app.general,
            blob(1000, 3),
            "application/zip",
            "exact",
        )
        .await;
    assert_eq!(exact.size, 1000);
    let over = app
        .upload_raw(
            &c.alice,
            app.general,
            AttachmentId::generate(),
            blob(1001, 3),
            "application/zip",
            "over",
        )
        .await;
    assert_eq!(over.status().as_u16(), 413);
    denied(over, ErrorCode::PayloadTooLarge).await;

    // A header that promises too much is refused before a byte of the body is read: the server
    // answers while the client is still silent.
    let id = AttachmentId::generate();
    let head = put_head(
        app,
        &c.alice,
        app.general,
        id,
        "Content-Length: 5000000\r\n",
    );
    let (status, answer) = raw_request(app.server.addr, &head, b"", false).await;
    assert_eq!(status, 413, "{answer}");
    assert!(answer.contains("payload_too_large"));
    // No length at all (a chunked body) is refused too.
    let id = AttachmentId::generate();
    let head = put_head(
        app,
        &c.alice,
        app.general,
        id,
        "Transfer-Encoding: chunked\r\n",
    );
    let (status, answer) =
        raw_request(app.server.addr, &head, b"5\r\nhello\r\n0\r\n\r\n", false).await;
    assert_eq!(status, 400, "{answer}");
    assert!(answer.contains("invalid_request") && answer.contains("Content-Length is required"));
    // A body that stops short of what the header said is not stored, and leaves nothing behind.
    let id = AttachmentId::generate();
    let head = put_head(app, &c.alice, app.general, id, "Content-Length: 900\r\n");
    let _ = raw_request(app.server.addr, &head, &blob(100, 4), true).await;
    eventually("the interrupted upload to be cleaned up", || {
        staged_files(&dir).is_empty()
    })
    .await;
    assert_eq!(rows(app).await, 1);
    assert_eq!(stored_files(&dir).len(), 1);
    // The default limit is the protocol's.
    assert_eq!(AttachmentOptions::default().max_bytes, MAX_ATTACHMENT_BYTES);
    finish(c).await;
}

#[sqlx::test]
async fn the_largest_file_streams_through_and_one_byte_more_is_refused_by_default(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let largest = blob(MAX_ATTACHMENT_BYTES as usize, 9);
    let attachment = app
        .upload(
            &c.alice,
            app.general,
            largest.clone(),
            "application/zip",
            "big.bin",
        )
        .await;
    assert_eq!(
        (attachment.size, attachment.sha256.clone()),
        (MAX_ATTACHMENT_BYTES, sha_hex(&largest))
    );
    let id = AttachmentId::generate();
    let head = put_head(
        app,
        &c.alice,
        app.general,
        id,
        &format!("Content-Length: {}\r\n", MAX_ATTACHMENT_BYTES + 1),
    );
    let (status, _) = raw_request(app.server.addr, &head, b"", false).await;
    assert_eq!(status, 413);
    let back = bytes_of(
        app.download_raw(&c.alice, attachment.id, false, false, &[])
            .await,
    )
    .await;
    assert!(back == largest, "the 25 MiB came back different");
    finish(c).await;
}

#[sqlx::test]
async fn uploading_twice_is_one_upload_and_an_id_cannot_be_reused_for_other_bytes(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let id = AttachmentId::generate();
    let bytes = blob(5000, 5);
    let first = app
        .upload_raw(
            &c.alice,
            app.general,
            id,
            bytes.clone(),
            "application/zip",
            "a.zip",
        )
        .await;
    let first = first.json::<AttachmentResponse>().await.unwrap().attachment;
    // The same id with the same bytes is the same attachment, and no second file.
    let again = app
        .upload_raw(
            &c.alice,
            app.general,
            id,
            bytes.clone(),
            "application/zip",
            "a.zip",
        )
        .await;
    assert_eq!(again.status().as_u16(), 200);
    assert_eq!(
        again.json::<AttachmentResponse>().await.unwrap().attachment,
        first
    );
    assert_eq!(
        (
            rows(app).await,
            stored_files(app.server.attachments_dir()).len()
        ),
        (1, 1)
    );
    // The same id with different bytes (of the same length, or not) is a conflict.
    denied(
        app.upload_raw(
            &c.alice,
            app.general,
            id,
            blob(5000, 6),
            "application/zip",
            "a.zip",
        )
        .await,
        ErrorCode::Conflict,
    )
    .await;
    denied(
        app.upload_raw(
            &c.alice,
            app.general,
            id,
            blob(4000, 5),
            "application/zip",
            "a.zip",
        )
        .await,
        ErrorCode::Conflict,
    )
    .await;
    // Someone else's id, or another conversation's, is a conflict that says no more.
    let team = app.create_channel(&c.alice, "team", "public").await;
    assert!(app.act(&c.bob, team.id, "join").await.status().is_success());
    let elsewhere = app
        .upload_raw(
            &c.alice,
            team.id,
            id,
            bytes.clone(),
            "application/zip",
            "a.zip",
        )
        .await;
    let (status_a, body_a) = api_error(elsewhere).await;
    let theirs = app
        .upload_raw(
            &c.bob,
            app.general,
            id,
            bytes.clone(),
            "application/zip",
            "a.zip",
        )
        .await;
    let (status_b, body_b) = api_error(theirs).await;
    assert_eq!((status_a, status_b), (409, 409));
    assert_eq!(body_a, body_b);
    // What is on a message can still be uploaded again by its owner (a retry after the send).
    let message = app
        .send_with_files(&c.alice, app.general, "here", &[id])
        .await;
    let late = app
        .upload_raw(&c.alice, app.general, id, bytes, "application/zip", "a.zip")
        .await;
    assert_eq!(late.status().as_u16(), 200);
    assert_eq!(files_of(&message), vec![id]);
    // A gone one cannot be brought back under its old id.
    app.delete(&c.alice, message.id).await;
    let gone = app
        .upload_raw(
            &c.alice,
            app.general,
            id,
            blob(5000, 5),
            "application/zip",
            "a.zip",
        )
        .await;
    denied(gone, ErrorCode::Conflict).await;
    finish(c).await;
}

#[sqlx::test]
async fn an_image_that_cannot_be_decoded_or_has_too_many_pixels_is_refused(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let mut bomb = png_bytes(1, 1, 255);
    bomb[16..20].copy_from_slice(&10_000u32.to_be_bytes());
    bomb[20..24].copy_from_slice(&10_000u32.to_be_bytes());
    let mut crc = u32::MAX;
    for byte in &bomb[12..29] {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    bomb[29..33].copy_from_slice(&(!crc).to_be_bytes());
    let mut truncated = png_bytes(400, 400, 255);
    truncated.truncate(truncated.len() / 2);
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "png signature and nothing else",
            b"\x89PNG\r\n\x1a\nthen garbage".to_vec(),
            "image/png",
        ),
        ("a pixel bomb", bomb, "image/png"),
        ("a truncated png", truncated, "image/png"),
        (
            "a jpeg that is not",
            b"\xff\xd8\xff\xe0 not jpeg data at all".to_vec(),
            "image/jpeg",
        ),
        (
            "a gif that is not",
            b"GIF89a garbage garbage".to_vec(),
            "image/gif",
        ),
        (
            "a webp that is not",
            b"RIFF\x10\0\0\0WEBPVP8 garbage garbage".to_vec(),
            "image/webp",
        ),
    ];
    for (what, bytes, claimed) in cases {
        let response = app
            .upload_raw(
                &c.alice,
                app.general,
                AttachmentId::generate(),
                bytes,
                claimed,
                "x",
            )
            .await;
        assert_eq!(response.status().as_u16(), 415, "{what}");
        denied(response, ErrorCode::UnsupportedMedia).await;
    }
    // Nothing of any of them was kept.
    assert_eq!(rows(app).await, 0);
    assert!(stored_files(app.server.attachments_dir()).is_empty());
    assert!(staged_files(app.server.attachments_dir()).is_empty());
    finish(c).await;
}

#[sqlx::test]
async fn uploads_of_one_id_racing_each_other_leave_one_file_that_matches_its_row(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    // Eight identical uploads at once: all succeed with the same attachment, and there is one.
    let id = AttachmentId::generate();
    let bytes = blob(20_000, 7);
    let responses = join_all((0..8).map(|_| {
        let (app, alice, bytes) = (&c.app, c.alice.clone(), bytes.clone());
        async move {
            app.upload_raw(&alice, app.general, id, bytes, "application/zip", "same")
                .await
        }
    }))
    .await;
    let mut seen = Vec::new();
    for response in responses {
        assert_eq!(response.status().as_u16(), 200);
        seen.push(
            response
                .json::<AttachmentResponse>()
                .await
                .unwrap()
                .attachment,
        );
    }
    assert!(seen.iter().all(|a| a == &seen[0]));
    assert_eq!(
        (
            rows(app).await,
            stored_files(app.server.attachments_dir()).len()
        ),
        (1, 1)
    );
    assert!(staged_files(app.server.attachments_dir()).is_empty());

    // Two different files with one id at once: one wins, the other is a conflict, and the stored
    // bytes are the winner's, whoever won.
    let id = AttachmentId::generate();
    let (a, b) = (blob(30_000, 11), blob(30_000, 12));
    let responses = join_all((0..8).map(|n| {
        let (app, alice, bytes) = (
            &c.app,
            c.alice.clone(),
            if n % 2 == 0 { a.clone() } else { b.clone() },
        );
        async move {
            (
                n % 2,
                app.upload_raw(&alice, app.general, id, bytes, "application/zip", "race")
                    .await,
            )
        }
    }))
    .await;
    let mut winners = Vec::new();
    for (variant, response) in responses {
        match response.status().as_u16() {
            200 => winners.push(variant),
            409 => {}
            other => panic!("unexpected status {other}"),
        }
    }
    assert!(
        !winners.is_empty() && winners.iter().all(|v| *v == winners[0]),
        "{winners:?}"
    );
    let winner = if winners[0] == 0 { &a } else { &b };
    let on_disk = bytes_of(app.download_raw(&c.alice, id, false, false, &[]).await).await;
    assert!(&on_disk == winner);
    let stored_hash: Vec<u8> = sqlx::query_scalar("SELECT sha256 FROM attachments WHERE id = $1")
        .bind(id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(stored_hash, Sha256::digest(winner).to_vec());
    assert!(staged_files(app.server.attachments_dir()).is_empty());
    every_row_has_its_file(app).await;
    finish(c).await;
}

#[sqlx::test]
async fn a_person_can_have_fifty_uploads_waiting_exactly_and_sending_frees_them(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    // Sixty at once: fifty get in, ten are refused, however they interleave.
    let responses = join_all((0..60).map(|n| {
        let (app, alice) = (&c.app, c.alice.clone());
        async move {
            let id = AttachmentId::generate();
            let response = app
                .upload_raw(
                    &alice,
                    app.general,
                    id,
                    blob(10, n as u8),
                    "application/zip",
                    "f",
                )
                .await;
            (id, response)
        }
    }))
    .await;
    let (mut held, mut refused) = (Vec::new(), 0);
    for (id, response) in responses {
        match response.status().as_u16() {
            200 => held.push(id),
            422 => {
                refused += 1;
                denied(response, ErrorCode::LimitReached).await;
            }
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!((held.len(), refused), (50, 10));
    // Someone else has their own fifty.
    app.upload(&c.bob, app.general, blob(10, 1), "application/zip", "f")
        .await;
    // Sending ten of them frees ten places.
    let message = app
        .send_with_files(&c.alice, app.general, "ten files", &held[..10])
        .await;
    assert_eq!(message.attachments.len(), 10);
    for _ in 0..10 {
        app.upload(&c.alice, app.general, blob(10, 2), "application/zip", "f")
            .await;
    }
    denied(
        app.upload_raw(
            &c.alice,
            app.general,
            AttachmentId::generate(),
            blob(10, 3),
            "application/zip",
            "f",
        )
        .await,
        ErrorCode::LimitReached,
    )
    .await;
    finish(c).await;
}

#[sqlx::test]
async fn two_hundred_megabytes_of_waiting_uploads_is_the_most(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let chunk = blob(24 * 1024 * 1024, 5);
    for _ in 0..8 {
        app.upload(
            &c.alice,
            app.general,
            chunk.clone(),
            "application/zip",
            "big",
        )
        .await;
    }
    // 192 MiB are waiting; another 24 would make 216. Refused before the body is read.
    let head = put_head(
        app,
        &c.alice,
        app.general,
        AttachmentId::generate(),
        &format!("Content-Length: {}\r\n", chunk.len()),
    );
    let (status, answer) = raw_request(app.server.addr, &head, b"", false).await;
    assert_eq!(status, 422, "{answer}");
    assert!(answer.contains("limit_reached"));
    // A smaller one still fits.
    app.upload(
        &c.alice,
        app.general,
        blob(1024 * 1024, 6),
        "application/zip",
        "small",
    )
    .await;
    finish(c).await;
}

/// An upload's last step runs under two advisory locks, one for its id and one for its uploader,
/// so that one id is written by one upload at a time and one person's limits are exact. The test
/// holds a lock itself, so the outcome never depends on timing: the upload must stay blocked, and
/// finish once the lock is let go.
#[sqlx::test]
async fn an_upload_takes_turns_by_id_and_by_person(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let holding = |name: String| {
        let pool = app.pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(name)
                .execute(&mut *tx)
                .await
                .unwrap();
            tx
        }
    };
    let start_upload = |who: &Account, id: AttachmentId| {
        let (http, url, token) = (
            app.http.clone(),
            app.url(&lazy_protocol::api::paths::attachment_upload(
                app.general,
                id,
            )),
            who.token.clone(),
        );
        tokio::spawn(async move {
            http.put(url)
                .bearer_auth(token)
                .header("content-type", "application/zip")
                .body(vec![7u8; 100])
                .send()
                .await
                .unwrap()
        })
    };
    // The same id: held, the upload waits; let go, it finishes.
    let id = AttachmentId::generate();
    let holder = holding(format!("attachment:{id}")).await;
    let mut upload = start_upload(&c.alice, id);
    assert!(
        blocked(&mut upload).await,
        "an upload should wait for its id's lock"
    );
    holder.commit().await.unwrap();
    assert_eq!(upload.await.unwrap().status().as_u16(), 200);

    // The same person: held, all of their uploads wait; someone else's do not.
    let holder = holding(format!("attachment-user:{}", c.alice.user.id)).await;
    let mut mine = start_upload(&c.alice, AttachmentId::generate());
    let theirs = start_upload(&c.bob, AttachmentId::generate());
    assert_eq!(theirs.await.unwrap().status().as_u16(), 200);
    assert!(
        blocked(&mut mine).await,
        "an upload should wait for its uploader's lock"
    );
    holder.commit().await.unwrap();
    assert_eq!(mine.await.unwrap().status().as_u16(), 200);
    every_row_has_its_file(app).await;
    finish(c).await;
}

/// Whether an upload is still waiting after a while that would have been plenty to finish it.
async fn blocked(upload: &mut tokio::task::JoinHandle<reqwest::Response>) -> bool {
    tokio::time::timeout(Duration::from_millis(600), upload)
        .await
        .is_err()
}

// --- Who may upload and download ---------------------------------------------------------------

#[sqlx::test]
async fn only_people_who_can_post_may_upload(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let hush = app.create_channel(&c.alice, "hush", "private").await;
    assert!(
        app.add_members(&c.alice, hush.id, &[&c.bob])
            .await
            .status()
            .is_success()
    );
    let team = app.create_channel(&c.alice, "team", "public").await;
    let put = |who: &Account, conversation: ConversationId| {
        let (app, who) = (&c.app, who.clone());
        async move {
            app.upload_raw(
                &who,
                conversation,
                AttachmentId::generate(),
                blob(10, 1),
                "application/zip",
                "f",
            )
            .await
        }
    };
    // A member can.
    assert_eq!(put(&c.bob, hush.id).await.status().as_u16(), 200);
    // Someone who is not in a private channel cannot see it, admin and owner included.
    for outsider in [&c.carol, &c.boss, &c.owner] {
        denied(put(outsider, hush.id).await, ErrorCode::NotFound).await;
    }
    // Someone who can read a public channel without being in it can read but not upload.
    denied(put(&c.carol, team.id).await, ErrorCode::Forbidden).await;
    // A person removed from a private channel is out of it.
    assert!(
        app.remove_member(&c.alice, hush.id, &c.bob)
            .await
            .status()
            .is_success()
    );
    denied(put(&c.bob, hush.id).await, ErrorCode::NotFound).await;
    // Nothing can be uploaded to an archived channel.
    assert_eq!(put(&c.alice, team.id).await.status().as_u16(), 200);
    assert!(
        app.act(&c.alice, team.id, "archive")
            .await
            .status()
            .is_success()
    );
    denied(put(&c.alice, team.id).await, ErrorCode::Forbidden).await;
    // A conversation that does not exist, and no sign-in.
    denied(
        put(&c.alice, ConversationId::generate()).await,
        ErrorCode::NotFound,
    )
    .await;
    let nobody = app
        .http
        .put(app.url(&lazy_protocol::api::paths::attachment_upload(
            app.general,
            AttachmentId::generate(),
        )))
        .body(vec![1u8, 2, 3])
        .send()
        .await
        .unwrap();
    denied(nobody, ErrorCode::Unauthenticated).await;
    finish(c).await;
}

#[sqlx::test]
async fn a_pending_file_is_its_uploaders_alone_and_a_sent_one_is_the_conversations(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let hush = app.create_channel(&c.alice, "hush", "private").await;
    assert!(
        app.add_members(&c.alice, hush.id, &[&c.bob])
            .await
            .status()
            .is_success()
    );
    let team = app.create_channel(&c.alice, "team", "public").await;
    let get = |who: &Account, id: AttachmentId, thumbnail: bool| {
        let (app, who) = (&c.app, who.clone());
        async move { app.download_raw(&who, id, thumbnail, false, &[]).await }
    };

    // Pending: the uploader reads it, and nobody else, members of the conversation included.
    let secret = app
        .upload(
            &c.bob,
            hush.id,
            png_bytes(50, 50, 255),
            "image/png",
            "plan.png",
        )
        .await;
    assert_eq!(get(&c.bob, secret.id, false).await.status().as_u16(), 200);
    assert_eq!(get(&c.bob, secret.id, true).await.status().as_u16(), 200);
    for other in [&c.alice, &c.carol, &c.boss, &c.owner] {
        denied(get(other, secret.id, false).await, ErrorCode::NotFound).await;
        denied(get(other, secret.id, true).await, ErrorCode::NotFound).await;
    }
    // Sent: whoever reads the conversation. A private channel stays private.
    app.send_with_files(&c.bob, hush.id, "the plan", &[secret.id])
        .await;
    assert_eq!(get(&c.alice, secret.id, false).await.status().as_u16(), 200);
    assert_eq!(get(&c.alice, secret.id, true).await.status().as_u16(), 200);
    for outsider in [&c.carol, &c.boss, &c.owner] {
        denied(get(outsider, secret.id, false).await, ErrorCode::NotFound).await;
        denied(get(outsider, secret.id, true).await, ErrorCode::NotFound).await;
    }
    // A public channel's files are readable by people who have not joined it.
    let open = app
        .upload(
            &c.alice,
            team.id,
            png_bytes(50, 50, 255),
            "image/png",
            "news.png",
        )
        .await;
    app.send_with_files(&c.alice, team.id, "news", &[open.id])
        .await;
    assert_eq!(get(&c.carol, open.id, false).await.status().as_u16(), 200);
    // Someone removed from the private channel loses its files.
    assert!(
        app.remove_member(&c.alice, hush.id, &c.bob)
            .await
            .status()
            .is_success()
    );
    denied(get(&c.bob, secret.id, false).await, ErrorCode::NotFound).await;
    // A file that never existed answers exactly as one nobody may read.
    let (status_a, body_a) = api_error(get(&c.carol, secret.id, false).await).await;
    let (status_b, body_b) = api_error(get(&c.carol, AttachmentId::generate(), false).await).await;
    assert_eq!((status_a, &body_a), (status_b, &body_b));
    let unsigned = app
        .http
        .get(app.url(&lazy_protocol::api::paths::attachment(open.id)))
        .send()
        .await
        .unwrap();
    denied(unsigned, ErrorCode::Unauthenticated).await;
    finish(c).await;
}

// --- Attaching to messages ---------------------------------------------------------------------

#[sqlx::test]
async fn a_message_carries_its_files_in_order_and_every_view_of_it_shows_them(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let cursor = app.head().await;
    let one = app
        .upload(
            &c.alice,
            app.general,
            png_bytes(300, 200, 255),
            "image/png",
            "one.png",
        )
        .await;
    let two = app
        .upload(
            &c.alice,
            app.general,
            b"two".to_vec(),
            "text/plain",
            "two.txt",
        )
        .await;
    let three = app
        .upload(
            &c.alice,
            app.general,
            b"%PDF-1.4 three".to_vec(),
            "application/pdf",
            "three.pdf",
        )
        .await;
    let sent = app
        .send_with_files(
            &c.alice,
            app.general,
            "three files",
            &[three.id, one.id, two.id],
        )
        .await;
    assert_eq!(
        sent.attachments,
        vec![three.clone(), one.clone(), two.clone()]
    );
    assert_eq!(sent.document.plain_text(), "three files");

    // History, GET, replay, and the status of the operation all show the same files.
    let page = app.history_ok(&c.bob, app.general, &[]).await;
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].attachments, sent.attachments);
    assert_eq!(
        app.get_message(&c.bob, sent.id).await.attachments,
        sent.attachments
    );
    let mut ws = app.connect_ws(&c.carol).await;
    ws.subscribe(app.workspace_id, cursor).await;
    let events: Vec<EventEnvelope> = ws.replay().await.0;
    let created: Vec<&Message> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::MessageCreated { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].attachments, sent.attachments);
    // Sending again with the same operation id returns the same message, once.
    let operation = OperationId::generate();
    let more = app
        .upload(
            &c.alice,
            app.general,
            b"more".to_vec(),
            "text/plain",
            "more.txt",
        )
        .await;
    let first = app
        .send_with_files_raw(
            &c.alice,
            app.general,
            operation,
            "retry me",
            &[more.id],
            None,
            false,
        )
        .await;
    let first = first
        .json::<lazy_protocol::SendMessageResponse>()
        .await
        .unwrap()
        .message;
    let head = app.head().await;
    let again = app
        .send_with_files_raw(
            &c.alice,
            app.general,
            operation,
            "retry me",
            &[more.id],
            None,
            false,
        )
        .await;
    assert_eq!(
        again
            .json::<lazy_protocol::SendMessageResponse>()
            .await
            .unwrap()
            .message,
        first
    );
    assert_eq!(app.head().await, head);
    assert_eq!(first.attachments, vec![more.clone()]);
    // The same operation id for other files is a conflict, not a second message.
    let other = app
        .upload(
            &c.alice,
            app.general,
            b"other".to_vec(),
            "text/plain",
            "other.txt",
        )
        .await;
    denied(
        app.send_with_files_raw(
            &c.alice,
            app.general,
            operation,
            "retry me",
            &[other.id],
            None,
            false,
        )
        .await,
        ErrorCode::Conflict,
    )
    .await;
    match app.operation(&c.alice, operation).await {
        lazy_protocol::OperationStatus::Committed { message } => assert_eq!(message, first),
        other => panic!("unexpected status {other:?}"),
    }
    finish(c).await;
}

#[sqlx::test]
async fn a_message_that_names_a_file_it_may_not_have_sends_nothing(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let team = app.create_channel(&c.alice, "team", "public").await;
    assert!(app.act(&c.bob, team.id, "join").await.status().is_success());
    let mine = app
        .upload(
            &c.alice,
            app.general,
            b"mine".to_vec(),
            "text/plain",
            "mine.txt",
        )
        .await;
    let theirs = app
        .upload(
            &c.bob,
            app.general,
            b"theirs".to_vec(),
            "text/plain",
            "theirs.txt",
        )
        .await;
    let elsewhere = app
        .upload(
            &c.alice,
            team.id,
            b"team".to_vec(),
            "text/plain",
            "team.txt",
        )
        .await;
    let head = app.head().await;
    let attempt = |files: Vec<AttachmentId>| {
        let (app, alice) = (&c.app, c.alice.clone());
        async move {
            app.send_with_files_raw(
                &alice,
                app.general,
                OperationId::generate(),
                "hello",
                &files,
                None,
                false,
            )
            .await
        }
    };

    // Someone else's, another conversation's, and one that does not exist: all the same refusal.
    let (status_a, body_a) = api_error(attempt(vec![mine.id, theirs.id]).await).await;
    let (status_b, body_b) = api_error(attempt(vec![mine.id, elsewhere.id]).await).await;
    let (status_c, body_c) =
        api_error(attempt(vec![mine.id, AttachmentId::generate()]).await).await;
    assert_eq!((status_a, status_b, status_c), (404, 404, 404));
    assert!(body_a == body_b && body_b == body_c);
    // A file named twice, and eleven files.
    denied(
        attempt(vec![mine.id, mine.id]).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    let eleven: Vec<AttachmentId> = (0..11).map(|_| AttachmentId::generate()).collect();
    denied(attempt(eleven).await, ErrorCode::InvalidRequest).await;
    // Nothing was sent, and mine is still waiting.
    assert_eq!(app.head().await, head);
    assert_eq!(
        app.history_ok(&c.alice, app.general, &[])
            .await
            .messages
            .len(),
        0
    );
    let message = app
        .send_with_files(&c.alice, app.general, "now", &[mine.id])
        .await;
    // A file already on a message cannot go on another.
    denied(attempt(vec![mine.id]).await, ErrorCode::InvalidRequest).await;
    assert_eq!(files_of(&message), vec![mine.id]);
    // Ten is fine.
    let mut ten = Vec::new();
    for n in 0..10 {
        ten.push(
            app.upload(
                &c.alice,
                app.general,
                vec![n as u8; 5],
                "application/zip",
                "f",
            )
            .await
            .id,
        );
    }
    assert_eq!(
        app.send_with_files(&c.alice, app.general, "ten", &ten)
            .await
            .attachments
            .len(),
        10
    );
    finish(c).await;
}

#[sqlx::test]
async fn a_message_can_be_only_files_but_never_nothing(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let team = app.create_channel(&c.alice, "team", "public").await;
    assert!(app.act(&c.bob, team.id, "join").await.status().is_success());
    let file = app
        .upload(&c.alice, team.id, b"x".to_vec(), "text/plain", "x.txt")
        .await;
    // Nothing at all: refused, wherever it is.
    let empty = |conversation, root: Option<&Message>, also| {
        let (app, alice) = (&c.app, c.alice.clone());
        let root = root.cloned();
        async move {
            app.send_with_files_raw(
                &alice,
                conversation,
                OperationId::generate(),
                "",
                &[],
                root.as_ref(),
                also,
            )
            .await
        }
    };
    denied(empty(team.id, None, false).await, ErrorCode::InvalidRequest).await;
    let root = app.send(&c.bob, team.id, "a root").await;
    denied(
        empty(team.id, Some(&root), false).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    denied(
        empty(team.id, Some(&root), true).await,
        ErrorCode::InvalidRequest,
    )
    .await;

    // Files alone are a message: it has an empty document, no text anywhere, and counts as unread.
    let only = app.send_with_files(&c.alice, team.id, "", &[file.id]).await;
    assert!(only.document.has_no_content() && only.document.plain_text().is_empty());
    let stored: (String, bool) =
        sqlx::query_as("SELECT plain_text, listed FROM messages WHERE id = $1")
            .bind(only.id.as_uuid())
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(stored, (String::new(), true));
    let state = app.state_of(&c.bob, team.id).await;
    assert_eq!((state.unread_count, state.mention_count), (1, 0));
    let history = app.history_ok(&c.bob, team.id, &[]).await;
    assert_eq!(
        history.messages.last().unwrap().attachments,
        only.attachments
    );
    // It can be reacted to and deleted like anything.
    assert_eq!(
        app.react(&c.bob, only.id, "👍", true).await.reactions.len(),
        1
    );
    assert!(app.delete(&c.alice, only.id).await.deleted);

    // In a thread, and also in the channel.
    let reply_file = app
        .upload(&c.alice, team.id, b"r".to_vec(), "text/plain", "r.txt")
        .await;
    let reply = app
        .send_with_files_raw(
            &c.alice,
            team.id,
            OperationId::generate(),
            "",
            &[reply_file.id],
            Some(&root),
            false,
        )
        .await;
    let reply = reply
        .json::<lazy_protocol::SendMessageResponse>()
        .await
        .unwrap()
        .message;
    assert_eq!(
        (reply.thread_root, files_of(&reply)),
        (Some(root.id), vec![reply_file.id])
    );
    let both_file = app
        .upload(&c.alice, team.id, b"b".to_vec(), "text/plain", "b.txt")
        .await;
    let both = app
        .send_with_files_raw(
            &c.alice,
            team.id,
            OperationId::generate(),
            "with words",
            &[both_file.id],
            Some(&root),
            true,
        )
        .await;
    let both = both
        .json::<lazy_protocol::SendMessageResponse>()
        .await
        .unwrap()
        .message;
    assert!(both.in_channel && files_of(&both) == vec![both_file.id]);
    // The thread's page, the channel, and the root's summary all know.
    let page = app.replies_ok(&c.bob, root.id, &[]).await;
    assert_eq!(
        page.messages
            .iter()
            .map(|m| m.attachments.len())
            .collect::<Vec<_>>(),
        vec![1, 1]
    );
    let channel = app.history_ok(&c.bob, team.id, &[]).await;
    assert!(
        channel
            .messages
            .iter()
            .any(|m| m.id == both.id && m.attachments.len() == 1)
    );
    assert!(channel.messages.iter().all(|m| m.id != reply.id));
    assert_eq!(
        app.get_message(&c.bob, root.id)
            .await
            .thread
            .unwrap()
            .reply_count,
        2
    );
    finish(c).await;
}

#[sqlx::test]
async fn words_can_be_taken_out_of_a_message_only_if_it_has_files(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let edit = |who: &Account, message: &Message, document: MessageDocument| {
        let (app, who, id, revision) = (&c.app, who.clone(), message.id, message.revision);
        async move {
            app.call(
                reqwest::Method::PATCH,
                &lazy_protocol::api::paths::message(id),
                &who,
                Some(serde_json::json!({
                    "operation_id": OperationId::generate(),
                    "base_revision": revision,
                    "document": document,
                })),
            )
            .await
        }
    };
    let file = app
        .upload(&c.alice, app.general, b"x".to_vec(), "text/plain", "x.txt")
        .await;
    let with_files = app
        .send_with_files(&c.alice, app.general, "words and a file", &[file.id])
        .await;
    let plain = app.send(&c.alice, app.general, "only words").await;

    // Without files there must be words.
    denied(
        edit(&c.alice, &plain, MessageDocument::empty()).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    // With them, the words may go: the files stay, and the edit says so.
    let emptied = edit(&c.alice, &with_files, MessageDocument::empty()).await;
    assert!(emptied.status().is_success());
    let emptied = emptied
        .json::<lazy_protocol::MessageResponse>()
        .await
        .unwrap()
        .message;
    assert!(emptied.document.has_no_content());
    assert_eq!((emptied.revision, files_of(&emptied)), (2, vec![file.id]));
    // And words can come back, to a message that had none. Editing the same empty text is a no-op.
    let same = edit(&c.alice, &emptied, MessageDocument::empty()).await;
    assert_eq!(
        same.json::<lazy_protocol::MessageResponse>()
            .await
            .unwrap()
            .message
            .revision,
        2
    );
    let restored = edit(
        &c.alice,
        &emptied,
        MessageDocument::from_plain_text("back").unwrap(),
    )
    .await;
    let restored = restored
        .json::<lazy_protocol::MessageResponse>()
        .await
        .unwrap()
        .message;
    assert_eq!(
        (
            restored.document.plain_text(),
            restored.revision,
            files_of(&restored)
        ),
        ("back".to_owned(), 3, vec![file.id])
    );
    // An edit's event carries the files, since the message in it is the whole message.
    let page = app.history_ok(&c.bob, app.general, &[]).await;
    assert!(
        page.messages
            .iter()
            .any(|m| m.id == with_files.id && m.attachments.len() == 1)
    );
    finish(c).await;
}

#[sqlx::test]
async fn a_webhook_cannot_attach_files(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let hook = app.create_webhook(&c.boss, app.general, "Deploys").await;
    let file = app
        .upload(&c.boss, app.general, b"x".to_vec(), "text/plain", "x.txt")
        .await;
    // A webhook that asks to attach a file posts its words, and the file stays where it was.
    let body = serde_json::json!({"text": "deployed", "attachment_ids": [file.id]}).to_string();
    let response = app.deliver_body_raw(&hook.url, body, &[]).await;
    assert_eq!(response.status().as_u16(), 200);
    let id = response
        .json::<lazy_protocol::IncomingWebhookResponse>()
        .await
        .unwrap()
        .message_id;
    assert!(app.get_message(&c.alice, id).await.attachments.is_empty());
    let on: Option<Uuid> = sqlx::query_scalar("SELECT message_id FROM attachments WHERE id = $1")
        .bind(file.id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(on, None, "still waiting");
    // Words are required: it cannot send an empty message either.
    denied(
        app.deliver_raw(&hook.url, "", &[]).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    finish(c).await;
}

// --- Downloads ---------------------------------------------------------------------------------

#[sqlx::test]
async fn a_download_says_what_it_is_how_to_treat_it_and_that_it_never_changes(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let picture = png_bytes(64, 48, 255);
    let image = app
        .upload(
            &c.alice,
            app.general,
            picture.clone(),
            "image/png",
            "café photo.png",
        )
        .await;
    let file = app
        .upload(
            &c.alice,
            app.general,
            b"plain words".to_vec(),
            "text/plain",
            "naïve notes.txt",
        )
        .await;
    let html = app
        .upload(
            &c.alice,
            app.general,
            b"<script>alert(1)</script>".to_vec(),
            "text/html",
            "page.html",
        )
        .await;
    app.send_with_files(
        &c.alice,
        app.general,
        "files",
        &[image.id, file.id, html.id],
    )
    .await;

    let header = |response: &reqwest::Response, name: &str| {
        response
            .headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
    };
    // An image is shown inline, with its name in full and in a safe fallback.
    let shown = app.download_raw(&c.bob, image.id, false, false, &[]).await;
    assert_eq!(shown.status().as_u16(), 200);
    assert_eq!(header(&shown, "content-type").as_deref(), Some("image/png"));
    assert_eq!(
        header(&shown, "content-disposition").as_deref(),
        Some("inline; filename=\"caf_ photo.png\"; filename*=UTF-8''caf%C3%A9%20photo.png")
    );
    assert_eq!(
        header(&shown, "x-content-type-options").as_deref(),
        Some("nosniff")
    );
    assert_eq!(
        header(&shown, "content-security-policy").as_deref(),
        Some("default-src 'none'; sandbox")
    );
    assert_eq!(
        header(&shown, "etag").as_deref(),
        Some(format!("\"{}\"", image.sha256).as_str())
    );
    assert_eq!(
        header(&shown, "cache-control").as_deref(),
        Some("private, max-age=31536000, immutable")
    );
    assert_eq!(
        header(&shown, "content-length").as_deref(),
        Some(picture.len().to_string().as_str())
    );
    assert_eq!(shown.bytes().await.unwrap().to_vec(), picture);
    // Anything else is a download, and active content is not served as itself.
    let text = app.download_raw(&c.bob, file.id, false, false, &[]).await;
    assert_eq!(header(&text, "content-type").as_deref(), Some("text/plain"));
    assert!(
        header(&text, "content-disposition")
            .unwrap()
            .starts_with("attachment; filename=\"na_ve notes.txt\"")
    );
    let page = app.download_raw(&c.bob, html.id, false, false, &[]).await;
    assert_eq!(
        header(&page, "content-type").as_deref(),
        Some("application/octet-stream")
    );
    assert!(
        header(&page, "content-disposition")
            .unwrap()
            .starts_with("attachment;")
    );
    // A thumbnail is its own thing, with its own validator.
    let thumb = app.download_raw(&c.bob, image.id, true, false, &[]).await;
    assert_eq!(
        header(&thumb, "etag").as_deref(),
        Some(format!("\"{}-thumb\"", image.sha256).as_str())
    );
    assert_eq!(
        header(&thumb, "x-content-type-options").as_deref(),
        Some("nosniff")
    );

    // A matching validator is a 304 with no body (and still needs access); a stale one is a 200.
    let etag = format!("\"{}\"", image.sha256);
    let unchanged = app
        .download_raw(&c.bob, image.id, false, false, &[("if-none-match", &etag)])
        .await;
    assert_eq!(unchanged.status().as_u16(), 304);
    assert_eq!(header(&unchanged, "etag").as_deref(), Some(etag.as_str()));
    assert!(unchanged.bytes().await.unwrap().is_empty());
    let weak = format!("W/{etag}");
    assert_eq!(
        app.download_raw(&c.bob, image.id, false, false, &[("if-none-match", &weak)])
            .await
            .status()
            .as_u16(),
        304
    );
    assert_eq!(
        app.download_raw(
            &c.bob,
            image.id,
            false,
            false,
            &[("if-none-match", "\"other\"")]
        )
        .await
        .status()
        .as_u16(),
        200
    );
    // A validator is no way round access: a pending file is still its uploader's alone.
    let pending = app
        .upload(&c.alice, app.general, b"p".to_vec(), "text/plain", "p.txt")
        .await;
    let pending_etag = format!("\"{}\"", pending.sha256);
    denied(
        app.download_raw(
            &c.carol,
            pending.id,
            false,
            false,
            &[("if-none-match", &pending_etag)],
        )
        .await,
        ErrorCode::NotFound,
    )
    .await;
    assert_eq!(
        app.download_raw(
            &c.alice,
            pending.id,
            false,
            false,
            &[("if-none-match", &pending_etag)]
        )
        .await
        .status()
        .as_u16(),
        304
    );
    // HEAD says the same without the body.
    let head = app.download_raw(&c.bob, image.id, false, true, &[]).await;
    assert_eq!(head.status().as_u16(), 200);
    assert_eq!(
        header(&head, "content-length").as_deref(),
        Some(picture.len().to_string().as_str())
    );
    assert_eq!(header(&head, "etag").as_deref(), Some(etag.as_str()));
    assert!(head.bytes().await.unwrap().is_empty());
    // Ranges are not supported: the whole file, as a 200.
    let ranged = app
        .download_raw(&c.bob, image.id, false, false, &[("range", "bytes=0-9")])
        .await;
    assert_eq!(ranged.status().as_u16(), 200);
    assert_eq!(ranged.bytes().await.unwrap().len(), picture.len());
    finish(c).await;
}

// --- Deleting and cleaning up -------------------------------------------------------------------

#[sqlx::test]
async fn deleting_a_message_ends_access_to_its_files_at_once_and_cleanup_removes_the_bytes(
    pool: PgPool,
) {
    let c = chat(pool).await;
    let app = &c.app;
    let dir = app.server.attachments_dir().to_owned();
    let image = app
        .upload(
            &c.alice,
            app.general,
            png_bytes(200, 100, 255),
            "image/png",
            "a.png",
        )
        .await;
    let note = app
        .upload(
            &c.alice,
            app.general,
            b"note".to_vec(),
            "text/plain",
            "n.txt",
        )
        .await;
    let keeper = app
        .upload(
            &c.alice,
            app.general,
            b"keep".to_vec(),
            "text/plain",
            "k.txt",
        )
        .await;
    let doomed = app
        .send_with_files(&c.alice, app.general, "going", &[image.id, note.id])
        .await;
    let kept = app
        .send_with_files(&c.alice, app.general, "staying", &[keeper.id])
        .await;
    assert_eq!(
        stored_files(&dir).len(),
        4,
        "two files, one thumbnail, one more file"
    );
    let store = FileStore::open_dir(&dir).unwrap();

    let tombstone = app.delete(&c.boss, doomed.id).await;
    assert!(tombstone.deleted && tombstone.attachments.is_empty());
    // Access ended in the same transaction, for everyone: bytes and thumbnails alike.
    for who in [&c.alice, &c.bob, &c.boss] {
        denied(
            app.download_raw(who, image.id, false, false, &[]).await,
            ErrorCode::NotFound,
        )
        .await;
        denied(
            app.download_raw(who, image.id, true, false, &[]).await,
            ErrorCode::NotFound,
        )
        .await;
        denied(
            app.download_raw(who, note.id, false, false, &[]).await,
            ErrorCode::NotFound,
        )
        .await;
    }
    // The files are still on disk until the job runs, and the other message's file is unaffected.
    assert_eq!(stored_files(&dir).len(), 4);
    let report = cleanup(&app.pool, &store, Duration::from_secs(24 * 3600))
        .await
        .unwrap();
    assert_eq!((report.removed, report.expired), (2, 0));
    assert_eq!(stored_files(&dir).len(), 1);
    assert_eq!(rows(app).await, 1);
    assert_eq!(
        bytes_of(app.download_raw(&c.bob, keeper.id, false, false, &[]).await).await,
        b"keep"
    );
    // Running it again finds nothing; deleting twice is the same.
    assert_eq!(
        cleanup(&app.pool, &store, Duration::from_secs(24 * 3600))
            .await
            .unwrap(),
        Default::default()
    );
    app.delete(&c.boss, doomed.id).await;
    // The message that stayed still has its file in every view.
    assert_eq!(app.get_message(&c.bob, kept.id).await.attachments.len(), 1);
    // The events that announced the deleted message no longer name its files.
    let events: Vec<String> =
        sqlx::query_scalar("SELECT payload::text FROM events WHERE message_id = $1")
            .bind(doomed.id.as_uuid())
            .fetch_all(&app.pool)
            .await
            .unwrap();
    assert!(
        !events.is_empty()
            && events
                .iter()
                .all(|e| !e.contains(&image.sha256) && !e.contains("a.png"))
    );
    finish(c).await;
}

#[sqlx::test]
async fn uploads_nobody_sent_expire_and_cleanup_removes_them(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let dir = app.server.attachments_dir().to_owned();
    let store = FileStore::open_dir(&dir).unwrap();
    let old = app
        .upload(
            &c.alice,
            app.general,
            png_bytes(40, 40, 255),
            "image/png",
            "old.png",
        )
        .await;
    let sent = app
        .upload(
            &c.alice,
            app.general,
            b"sent".to_vec(),
            "text/plain",
            "sent.txt",
        )
        .await;
    app.send_with_files(&c.alice, app.general, "this one is sent", &[sent.id])
        .await;
    assert_eq!(stored_files(&dir).len(), 3);

    // Within its time, nothing is removed.
    let report = cleanup(&app.pool, &store, Duration::from_secs(24 * 3600))
        .await
        .unwrap();
    assert_eq!((report.expired, report.removed), (0, 0));
    // Past it, a pending upload goes; one on a message does not, however old.
    let report = cleanup(&app.pool, &store, Duration::ZERO).await.unwrap();
    assert_eq!((report.expired, report.removed), (1, 1));
    assert_eq!(stored_files(&dir).len(), 1, "the sent file only");
    denied(
        app.download_raw(&c.alice, old.id, false, false, &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.download_raw(&c.alice, old.id, true, false, &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.send_with_files_raw(
            &c.alice,
            app.general,
            OperationId::generate(),
            "late",
            &[old.id],
            None,
            false,
        )
        .await,
        ErrorCode::NotFound,
    )
    .await;
    assert_eq!(
        bytes_of(app.download_raw(&c.bob, sent.id, false, false, &[]).await).await,
        b"sent"
    );
    // The place it held is free again.
    assert_eq!(rows(app).await, 1);
    finish(c).await;
}

#[sqlx::test]
async fn the_sweep_removes_files_and_uploads_no_row_accounts_for_but_only_old_ones(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let dir = app.server.attachments_dir().to_owned();
    let store = FileStore::open_dir(&dir).unwrap();
    let real = app
        .upload(
            &c.alice,
            app.general,
            png_bytes(40, 40, 255),
            "image/png",
            "real.png",
        )
        .await;
    // A file with no row (a crash between the rename and the commit), its thumbnail, and an upload
    // that was never finished.
    let orphan = Uuid::now_v7();
    let staged = store
        .stage(
            orphan,
            Box::pin(futures_util::stream::iter(vec![Ok(
                bytes::Bytes::from_static(b"orphan"),
            )])),
            100,
        )
        .await
        .unwrap();
    store
        .commit(&staged.token, ObjectKey::original(orphan))
        .await
        .unwrap();
    store
        .put_small(ObjectKey::thumbnail(orphan), b"thumb".to_vec())
        .await
        .unwrap();
    let unfinished = store
        .stage(
            Uuid::now_v7(),
            Box::pin(futures_util::stream::iter(vec![Ok(
                bytes::Bytes::from_static(b"half"),
            )])),
            100,
        )
        .await
        .unwrap();
    assert_eq!((stored_files(&dir).len(), staged_files(&dir).len()), (4, 1));

    // Too young: a real upload has exactly these things for a moment, so they are left alone.
    let report = sweep_orphans(&app.pool, &store, Duration::from_secs(3600))
        .await
        .unwrap();
    assert_eq!((report.orphans, report.stale_uploads), (0, 0));
    assert_eq!((stored_files(&dir).len(), staged_files(&dir).len()), (4, 1));
    // Old enough: the orphan, its thumbnail, and the unfinished upload go; the real one stays.
    let report = sweep_orphans(&app.pool, &store, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!((report.orphans, report.stale_uploads), (2, 1));
    assert_eq!((stored_files(&dir).len(), staged_files(&dir).len()), (2, 0));
    assert_eq!(
        app.download_raw(&c.alice, real.id, false, false, &[])
            .await
            .status()
            .as_u16(),
        200
    );
    assert_eq!(
        app.download_raw(&c.alice, real.id, true, false, &[])
            .await
            .status()
            .as_u16(),
        200
    );
    let _ = unfinished;
    finish(c).await;
}

#[sqlx::test]
async fn a_failure_between_the_file_and_its_row_leaves_a_file_the_sweep_can_account_for(
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
    let dir = app.server.attachments_dir().to_owned();
    let store = FileStore::open_dir(&dir).unwrap();

    // The upload fails as a crash after storing the files would: nothing is acknowledged, there is
    // no row (never a row without its file), and the file waits for the sweep.
    let id = AttachmentId::generate();
    let response = app
        .upload_raw(
            &c.alice,
            app.general,
            id,
            png_bytes(30, 30, 255),
            "image/png",
            "x.png",
        )
        .await;
    assert_eq!(response.status().as_u16(), 500);
    assert_eq!(rows(app).await, 0);
    assert_eq!(
        (stored_files(&dir).len(), staged_files(&dir).len()),
        (2, 0),
        "the file and its thumbnail"
    );
    denied(
        app.download_raw(&c.alice, id, false, false, &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    let report = sweep_orphans(&app.pool, &store, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(report.orphans, 2);
    assert!(stored_files(&dir).is_empty());
    every_row_has_its_file(app).await;
    finish(c).await;
}

// --- Version -----------------------------------------------------------------------------------

#[sqlx::test]
async fn the_server_reports_a_protocol_version_that_has_attachments(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    const { assert!(PROTOCOL_VERSION >= 8) };
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
