//! Profiles and avatars (D23): changing a display name or picture, announcing it to every
//! workspace the person is an active member of and no other, how pictures are made and who may
//! fetch them, and the locking that keeps every workspace's log current under races.

mod common;

use std::time::Duration;

use common::*;
use futures_util::future::join_all;
use image::ImageEncoder;
use lazy_domain::workspace::{AVATAR_SIZE_PX, MAX_AVATAR_UPLOAD_BYTES};
use lazy_domain::{EventSeq, UserId, WorkspaceId};
use lazy_protocol::{ErrorCode, Event};
use sqlx::PgPool;

async fn denied(response: reqwest::Response, code: ErrorCode) {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
}

fn header(response: &reqwest::Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .map(|value| value.to_str().unwrap().to_owned())
        .unwrap_or_default()
}

fn decode(png: &[u8]) -> image::RgbaImage {
    image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .expect("a png")
        .to_rgba8()
}

/// A PNG of `width` by `height`: red left of x = 50, green up to x = width - 50, blue after.
fn banded_png(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbaImage::from_fn(width, height, |x, _| {
        if x < 50 {
            image::Rgba([230, 20, 20, 255])
        } else if x < width - 50 {
            image::Rgba([20, 230, 20, 255])
        } else {
            image::Rgba([20, 20, 230, 255])
        }
    });
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            image.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
    out
}

fn gif_bytes(width: u32, height: u32) -> Vec<u8> {
    let mut gif = Vec::new();
    image::codecs::gif::GifEncoder::new(&mut gif)
        .encode_frame(image::Frame::new(image::RgbaImage::from_pixel(
            width,
            height,
            image::Rgba([200, 60, 60, 255]),
        )))
        .unwrap();
    gif
}

fn webp_bytes(width: u32, height: u32) -> Vec<u8> {
    let mut webp = Vec::new();
    let pixels = image::RgbaImage::from_pixel(width, height, image::Rgba([60, 60, 200, 255]));
    image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
        .write_image(
            pixels.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
    webp
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A tiny PNG whose header claims `side` by `side` pixels, with no pixel data to match.
fn pixel_bomb(side: u32) -> Vec<u8> {
    let mut bomb = png_bytes(1, 1, 255);
    bomb[16..20].copy_from_slice(&side.to_be_bytes());
    bomb[20..24].copy_from_slice(&side.to_be_bytes());
    let checksum = crc32(&bomb[12..29]);
    bomb[29..33].copy_from_slice(&checksum.to_be_bytes());
    bomb
}

/// A request written by hand, for what the client library will not do (a body of no stated length).
async fn raw_request(addr: std::net::SocketAddr, head: &str, body: &[u8]) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(b"\r\n").await.unwrap();
    let _ = stream.write_all(body).await;
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut answer)).await;
    let text = String::from_utf8_lossy(&answer).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, text)
}

async fn db_profile(app: &App, user: UserId) -> (String, u32) {
    let (name, version): (String, i32) =
        sqlx::query_as("SELECT display_name, avatar_version FROM users WHERE id = $1")
            .bind(user.as_uuid())
            .fetch_one(&app.pool)
            .await
            .unwrap();
    (name, version as u32)
}

/// What a client holding `start` (the person's profile as the workspace's bootstrap or join gave
/// it, at event position `after`) would believe after applying the workspace's log from there.
async fn log_view(
    app: &App,
    workspace: WorkspaceId,
    user: UserId,
    start: (String, u32),
    after: EventSeq,
) -> (String, u32) {
    let rows: Vec<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload FROM events WHERE workspace_id = $1 AND seq > $2 ORDER BY seq",
    )
    .bind(workspace.as_uuid())
    .bind(after.get())
    .fetch_all(&app.pool)
    .await
    .unwrap();
    let mut view = start;
    for payload in rows {
        match serde_json::from_value::<Event>(payload).unwrap() {
            Event::UserProfileChanged {
                user_id,
                display_name,
                avatar_version,
            } if user_id == user => view = (display_name.to_string(), avatar_version),
            Event::MemberJoined { user: joined } if joined.id == user => {
                view = (joined.display_name.to_string(), joined.avatar_version)
            }
            _ => {}
        }
    }
    view
}

fn profile_of(workspace: &lazy_protocol::WorkspaceBootstrap, user: UserId) -> (String, u32) {
    let member = workspace
        .members
        .iter()
        .find(|m| m.id == user)
        .expect("a member");
    (member.display_name.to_string(), member.avatar_version)
}

// --- The display name ---------------------------------------------------------------------------

#[sqlx::test]
async fn a_new_name_is_announced_to_every_workspace_the_person_is_active_in_and_no_other(
    pool: PgPool,
) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let carol = app.register("carol").await;
    // Alice is in A (seeded) and B (hers); in D she was deactivated; C is not hers.
    let b = app.create_workspace(&alice, "B").await;
    let c = app.create_workspace(&carol, "C").await;
    let d = app.create_workspace(&carol, "D").await;
    let invite = app
        .invitation_to(&carol, d.workspace.id, serde_json::json!({}))
        .await;
    app.accept(&alice, invite.code.expose()).await;
    assert!(
        app.set_deactivated_raw(&carol, d.workspace.id, alice.user.id, true)
            .await
            .status()
            .is_success()
    );
    let invite = app
        .invitation_to(&alice, b.workspace.id, serde_json::json!({}))
        .await;
    app.accept(&bob, invite.code.expose()).await;

    let heads = [
        app.head_of(app.workspace_id).await,
        app.head_of(b.workspace.id).await,
        app.head_of(c.workspace.id).await,
        app.head_of(d.workspace.id).await,
    ];
    let mut bob_in_a = app.connect_ws(&bob).await;
    bob_in_a.subscribe(app.workspace_id, heads[0]).await;
    bob_in_a.replay().await;
    let mut bob_in_b = app.connect_ws(&bob).await;
    bob_in_b.subscribe(b.workspace.id, heads[1]).await;
    bob_in_b.replay().await;

    let renamed = app.rename_me(&alice, "Alice Liddell").await;
    assert_eq!(renamed.display_name.to_string(), "Alice Liddell");
    assert_eq!(renamed.id, alice.user.id);
    assert_eq!(renamed.avatar_version, 0);
    assert!(renamed.status.is_none() && !renamed.deactivated);

    for (workspace, head, expect) in [
        (app.workspace_id, heads[0], 1),
        (b.workspace.id, heads[1], 1),
        (c.workspace.id, heads[2], 0),
        (d.workspace.id, heads[3], 0),
    ] {
        let events = app.profile_events(workspace, head).await;
        assert_eq!(events.len(), expect, "workspace {workspace}");
        for (_, user, name, version) in events {
            assert_eq!(
                (user, name.as_str(), version),
                (alice.user.id, "Alice Liddell", 0)
            );
        }
    }
    // Live: bob hears it once in each workspace he is in.
    for (ws, workspace) in [
        (&mut bob_in_a, app.workspace_id),
        (&mut bob_in_b, b.workspace.id),
    ] {
        let head = app.head_of(workspace).await;
        let events = ws.events_through(head).await;
        assert!(
            matches!(
                &events.last().unwrap().event,
                Event::UserProfileChanged { user_id, display_name, avatar_version: 0 }
                    if *user_id == alice.user.id && display_name.as_str() == "Alice Liddell"
            ),
            "{events:?}"
        );
    }
    // A bootstrap shows the new name everywhere she is a member.
    for workspace in app.bootstrap_ok(&bob).await.workspaces {
        assert_eq!(profile_of(&workspace, alice.user.id).0, "Alice Liddell");
    }

    // Nothing changed means nothing announced: the same name, no fields, and an empty object.
    let heads_after: Vec<EventSeq> = vec![
        app.head_of(app.workspace_id).await,
        app.head_of(b.workspace.id).await,
        app.head_of(c.workspace.id).await,
        app.head_of(d.workspace.id).await,
    ];
    app.rename_me(&alice, "Alice Liddell").await;
    for body in [
        serde_json::json!({}),
        serde_json::json!({"display_name": null}),
    ] {
        let response = app.patch_me_raw(&alice, body).await;
        assert!(response.status().is_success());
        let user = response
            .json::<lazy_protocol::ProfileResponse>()
            .await
            .unwrap()
            .user;
        assert_eq!(user.display_name.to_string(), "Alice Liddell");
    }
    let heads_again = vec![
        app.head_of(app.workspace_id).await,
        app.head_of(b.workspace.id).await,
        app.head_of(c.workspace.id).await,
        app.head_of(d.workspace.id).await,
    ];
    assert_eq!(heads_again, heads_after);

    // Names that are not names are refused, and signing in is needed.
    denied(
        app.patch_me_raw(&alice, serde_json::json!({"display_name": ""}))
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    denied(
        app.patch_me_raw(&alice, serde_json::json!({"display_name": "x".repeat(500)}))
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    let anonymous = app
        .http
        .patch(app.url(lazy_protocol::api::paths::ME_ROUTE))
        .json(&serde_json::json!({"display_name": "Nobody"}))
        .send()
        .await
        .unwrap();
    denied(anonymous, ErrorCode::Unauthenticated).await;
    assert_eq!(db_profile(&app, alice.user.id).await.0, "Alice Liddell");
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn every_place_a_person_is_shown_carries_their_avatar_version(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let set = app.set_avatar(&alice, png_bytes(64, 64, 255)).await;
    assert_eq!(set.avatar_version, 1);

    // Sign-in, the bootstrap (self and members), the directory, a profile, and a status.
    let login: lazy_protocol::SessionResponse =
        app.try_login("alice", PASSWORD).await.json().await.unwrap();
    assert_eq!(login.user.avatar_version, 1);
    let boot = app.bootstrap_ok(&alice).await;
    assert_eq!(boot.user.avatar_version, 1);
    assert_eq!(profile_of(&boot.workspaces[0], alice.user.id).1, 1);
    assert_eq!(profile_of(&boot.workspaces[0], bob.user.id).1, 0);
    let people = app.people(&bob, &[]).await;
    let versions: Vec<(String, u32)> = people
        .people
        .iter()
        .map(|p| (p.user.username.to_string(), p.user.avatar_version))
        .collect();
    assert_eq!(versions, [("alice".to_owned(), 1), ("bob".to_owned(), 0)]);
    let person: lazy_protocol::PersonResponse = app
        .person_raw(&bob, app.workspace_id, alice.user.id)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(person.person.user.avatar_version, 1);
    assert_eq!(app.set_status(&alice, "Busy", None).await.avatar_version, 1);

    // Joining another workspace announces the person with the picture they have.
    let b = app.create_workspace(&bob, "B").await;
    let invite = app
        .invitation_to(&bob, b.workspace.id, serde_json::json!({}))
        .await;
    let joined = app.accept(&alice, invite.code.expose()).await;
    assert_eq!(profile_of(&joined, alice.user.id).1, 1);
    let log = app.profile_events(b.workspace.id, EventSeq::ZERO).await;
    assert!(log.is_empty());
    let rows: Vec<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload FROM events WHERE workspace_id = $1 AND kind = 'member_joined'",
    )
    .bind(b.workspace.id.as_uuid())
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["user"]["avatar_version"], 1);
    app.server.shutdown().await.unwrap();
}

// --- Making an avatar ---------------------------------------------------------------------------

#[sqlx::test]
async fn an_avatar_is_the_centre_square_of_the_picture_at_the_set_size(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let head = app.head_of(app.workspace_id).await;
    // 300 by 200: the centre 200 by 200 is the green band, whatever the sides were.
    let user = app.set_avatar(&alice, banded_png(300, 200)).await;
    assert_eq!(user.avatar_version, 1);
    let events = app.profile_events(app.workspace_id, head).await;
    assert_eq!(events.len(), 1);
    assert_eq!(
        (events[0].1, events[0].2.as_str(), events[0].3),
        (alice.user.id, "User alice", 1)
    );

    let response = app.avatar_raw(&bob, alice.user.id, Some(1), &[]).await;
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(header(&response, "content-type"), "image/png");
    assert_eq!(header(&response, "x-content-type-options"), "nosniff");
    let length = header(&response, "content-length");
    let bytes = response.bytes().await.unwrap();
    assert_eq!(length, bytes.len().to_string());
    let picture = decode(&bytes);
    assert_eq!(picture.dimensions(), (AVATAR_SIZE_PX, AVATAR_SIZE_PX));
    for (x, y) in [(2, 2), (128, 128), (253, 2), (2, 253), (253, 253)] {
        let [r, g, b, a] = picture.get_pixel(x, y).0;
        assert!(
            g > 200 && r < 60 && b < 60 && a == 255,
            "pixel {x},{y} is {r},{g},{b},{a}"
        );
    }

    // A picture smaller than the square is enlarged to it.
    let user = app.set_avatar(&alice, png_bytes(16, 16, 255)).await;
    assert_eq!(user.avatar_version, 2);
    let bytes = app
        .avatar_raw(&bob, alice.user.id, Some(2), &[])
        .await
        .bytes()
        .await
        .unwrap();
    assert_eq!(
        decode(&bytes).dimensions(),
        (AVATAR_SIZE_PX, AVATAR_SIZE_PX)
    );
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_photos_orientation_is_applied_before_it_is_cut(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let mut version = 0;
    // Left half red, right half blue, with the camera's rotation. Shown upright:
    // 1: left red, right blue. 6 (a quarter turn clockwise): red on top, blue below.
    // 8 (a quarter turn the other way): blue on top, red below. 3 (half a turn): left blue.
    for (orientation, a_point, b_point) in [
        (1u16, (10u32, 128u32), (245u32, 128u32)),
        (6, (128, 10), (128, 245)),
        (8, (128, 245), (128, 10)),
        (3, (245, 128), (10, 128)),
    ] {
        let (width, height) = if matches!(orientation, 6 | 8) {
            (240, 120)
        } else {
            (120, 240)
        };
        let user = app
            .set_avatar(&alice, jpeg_with_orientation(width, height, orientation))
            .await;
        version += 1;
        assert_eq!(user.avatar_version, version);
        let bytes = app
            .avatar_raw(&alice, alice.user.id, Some(version), &[])
            .await
            .bytes()
            .await
            .unwrap();
        let picture = decode(&bytes);
        assert_eq!(picture.dimensions(), (AVATAR_SIZE_PX, AVATAR_SIZE_PX));
        let red = picture.get_pixel(a_point.0, a_point.1).0;
        let blue = picture.get_pixel(b_point.0, b_point.1).0;
        assert!(
            red[0] > 150 && red[2] < 100,
            "orientation {orientation}: expected red at {a_point:?}, got {red:?}"
        );
        assert!(
            blue[2] > 150 && blue[0] < 100,
            "orientation {orientation}: expected blue at {b_point:?}, got {blue:?}"
        );
    }
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn gif_and_webp_avatars_are_accepted_as_their_first_frame(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    assert_eq!(
        app.set_avatar(&alice, gif_bytes(40, 30))
            .await
            .avatar_version,
        1
    );
    assert_eq!(
        app.set_avatar(&alice, webp_bytes(30, 50))
            .await
            .avatar_version,
        2
    );
    let bytes = app
        .avatar_raw(&alice, alice.user.id, Some(2), &[])
        .await
        .bytes()
        .await
        .unwrap();
    let picture = decode(&bytes);
    assert_eq!(picture.dimensions(), (AVATAR_SIZE_PX, AVATAR_SIZE_PX));
    let [r, _, b, _] = picture.get_pixel(128, 128).0;
    assert!(b > 150 && r < 100, "the webp's colour, got {r},{b}");
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn what_is_not_a_usable_picture_is_refused_and_changes_nothing(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let head = app.head_of(app.workspace_id).await;

    // Over the limit by its stated length: refused before any of it is read.
    denied(
        app.set_avatar_raw(&alice, vec![0u8; MAX_AVATAR_UPLOAD_BYTES + 1], "image/png")
            .await,
        ErrorCode::PayloadTooLarge,
    )
    .await;
    // Over the limit with no stated length (a chunked body): cut off as it passes the limit.
    let chunk = vec![7u8; 64 * 1024];
    let mut body = Vec::new();
    for _ in 0..(MAX_AVATAR_UPLOAD_BYTES / chunk.len() + 8) {
        body.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        body.extend_from_slice(&chunk);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"0\r\n\r\n");
    let head_text = format!(
        "PUT {} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: image/png\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n",
        lazy_protocol::api::paths::ME_AVATAR_ROUTE,
        app.server.addr,
        alice.token,
    );
    let (status, answer) = raw_request(app.server.addr, &head_text, &body).await;
    assert_eq!(status, 413, "{answer}");
    assert!(answer.contains("payload_too_large"), "{answer}");

    // Not images at all, however they are labelled; empty; a PDF; a cut-off PNG.
    let png = png_bytes(64, 64, 255);
    for (what, bytes, label) in [
        ("garbage", vec![0x13; 4096], "image/png"),
        ("text", b"hello, I am a picture".to_vec(), "image/jpeg"),
        ("empty", Vec::new(), "image/png"),
        ("a pdf", b"%PDF-1.7\n1 0 obj\n".to_vec(), "application/pdf"),
        ("a cut-off png", png[..png.len() / 2].to_vec(), "image/png"),
        ("a png header only", png[..33].to_vec(), "image/png"),
    ] {
        let (status, error) = api_error(app.set_avatar_raw(&alice, bytes, label).await).await;
        assert_eq!(
            (status, error.code),
            (415, ErrorCode::UnsupportedMedia),
            "{what}: {error}"
        );
    }
    // A decompression bomb: a header claiming far more pixels than the limit. Refused from the
    // header alone, quickly, without decoding (the pixels are not even in the file).
    let started = std::time::Instant::now();
    for side in [20_000, 100_000] {
        let (status, error) = api_error(
            app.set_avatar_raw(&alice, pixel_bomb(side), "image/png")
                .await,
        )
        .await;
        assert_eq!((status, error.code), (415, ErrorCode::UnsupportedMedia));
        assert!(error.message.contains("pixels"), "{error}");
    }
    assert!(started.elapsed() < Duration::from_secs(5));

    // Nothing changed: no event, no version, no picture.
    assert_eq!(app.head_of(app.workspace_id).await, head);
    assert_eq!(db_profile(&app, alice.user.id).await.1, 0);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM user_avatars")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    // Signing in is needed.
    let anonymous = app
        .http
        .put(app.url(lazy_protocol::api::paths::ME_AVATAR_ROUTE))
        .body(png)
        .send()
        .await
        .unwrap();
    denied(anonymous, ErrorCode::Unauthenticated).await;
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn the_content_type_is_only_a_hint(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let mut version = 0;
    for (bytes, label) in [
        (png_bytes(64, 64, 255), "text/plain"),
        (png_bytes(64, 64, 200), "image/jpeg"),
        (jpeg_with_orientation(64, 64, 1), "image/png"),
        (gif_bytes(20, 20), "application/pdf"),
        (webp_bytes(20, 21), ""),
    ] {
        let response = app.set_avatar_raw(&alice, bytes, label).await;
        assert_eq!(response.status().as_u16(), 200, "labelled {label:?}");
        version += 1;
        assert_eq!(
            response
                .json::<lazy_protocol::ProfileResponse>()
                .await
                .unwrap()
                .user
                .avatar_version,
            version
        );
    }
    app.server.shutdown().await.unwrap();
}

// --- Fetching an avatar -------------------------------------------------------------------------

#[sqlx::test]
async fn an_avatar_is_served_only_through_a_shared_active_workspace(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let carol = app.register("carol").await;
    // dave is only in B (alice's second workspace); frank in neither of alice's workspaces.
    let b = app.create_workspace(&alice, "B").await;
    let invite = app
        .invitation_to(&alice, b.workspace.id, serde_json::json!({}))
        .await;
    let dave = {
        let response = app
            .try_register_with("dave", PASSWORD, Some(invite.code.expose()))
            .await;
        let session: lazy_protocol::SessionResponse = response.json().await.unwrap();
        Account {
            token: session.token.expose().to_owned(),
            user: session.user,
        }
    };
    let frank = {
        let other = app.create_workspace(&carol, "Other").await;
        let invite = app
            .invitation_to(&carol, other.workspace.id, serde_json::json!({}))
            .await;
        let response = app
            .try_register_with("frank", PASSWORD, Some(invite.code.expose()))
            .await;
        let session: lazy_protocol::SessionResponse = response.json().await.unwrap();
        Account {
            token: session.token.expose().to_owned(),
            user: session.user,
        }
    };
    app.set_avatar(&alice, png_bytes(64, 64, 255)).await;
    let id = alice.user.id;
    let status = |response: reqwest::Response| response.status().as_u16();

    // Herself; someone in A with her; someone in B with her.
    assert_eq!(status(app.avatar_raw(&alice, id, Some(1), &[]).await), 200);
    assert_eq!(status(app.avatar_raw(&bob, id, Some(1), &[]).await), 200);
    assert_eq!(status(app.avatar_raw(&dave, id, Some(1), &[]).await), 200);
    // Strangers, a person with no shared workspace, and a person who does not exist: one answer.
    let (s1, stranger) = api_error(app.avatar_raw(&frank, id, Some(1), &[]).await).await;
    assert_error(s1, &stranger, ErrorCode::NotFound);
    let (s2, nobody) =
        api_error(app.avatar_raw(&bob, UserId::generate(), Some(1), &[]).await).await;
    assert_error(s2, &nobody, ErrorCode::NotFound);
    let (s3, no_picture) = api_error(app.avatar_raw(&alice, bob.user.id, Some(1), &[]).await).await;
    assert_error(s3, &no_picture, ErrorCode::NotFound);
    assert_eq!(stranger, nobody);
    assert_eq!(stranger, no_picture);
    // Signing in is needed.
    let anonymous = app
        .http
        .get(app.url(&lazy_protocol::api::paths::user_avatar(id, 1)))
        .send()
        .await
        .unwrap();
    denied(anonymous, ErrorCode::Unauthenticated).await;

    // Deactivated viewer: bob loses A, so loses her. Reactivated, he has it again.
    app.deactivate(&alice, &bob, true).await;
    assert_eq!(status(app.avatar_raw(&bob, id, Some(1), &[]).await), 404);
    app.deactivate(&alice, &bob, false).await;
    assert_eq!(status(app.avatar_raw(&bob, id, Some(1), &[]).await), 200);
    // Deactivated owner of the picture: in A she is nobody's colleague, but B still shares.
    assert!(
        app.set_role_raw(&alice, app.workspace_id, bob.user.id, "owner")
            .await
            .status()
            .is_success()
    );
    assert!(
        app.set_deactivated_raw(&bob, app.workspace_id, id, true)
            .await
            .status()
            .is_success()
    );
    assert_eq!(status(app.avatar_raw(&bob, id, Some(1), &[]).await), 404);
    assert_eq!(status(app.avatar_raw(&carol, id, Some(1), &[]).await), 404);
    assert_eq!(status(app.avatar_raw(&dave, id, Some(1), &[]).await), 200);
    // Her own picture stays hers while she is deactivated in A, and even with no active
    // workspace at all; then nobody else may see it.
    assert_eq!(status(app.avatar_raw(&alice, id, Some(1), &[]).await), 200);
    sqlx::query("UPDATE workspace_members SET deactivated_at = now() WHERE user_id = $1")
        .bind(id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    assert_eq!(status(app.avatar_raw(&alice, id, Some(1), &[]).await), 200);
    assert_eq!(status(app.avatar_raw(&dave, id, Some(1), &[]).await), 404);

    // An app has no picture to serve even if a row said otherwise.
    let hook = app
        .create_webhook(&bob, app.general, "Builds")
        .await
        .webhook
        .bot_user_id;
    sqlx::query("UPDATE users SET avatar_version = 1 WHERE id = $1")
        .bind(hook.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_avatars (user_id, version, png) VALUES ($1, 1, $2)")
        .bind(hook.as_uuid())
        .bind(png_bytes(8, 8, 255))
        .execute(&app.pool)
        .await
        .unwrap();
    assert_eq!(status(app.avatar_raw(&bob, hook, Some(1), &[]).await), 404);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn the_picture_is_cached_for_good_only_under_its_own_version(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    app.set_avatar(&alice, png_bytes(64, 64, 255)).await;
    let id = alice.user.id;

    let current = app.avatar_raw(&bob, id, Some(1), &[]).await;
    assert_eq!(current.status().as_u16(), 200);
    assert_eq!(
        header(&current, "cache-control"),
        "private, max-age=31536000, immutable"
    );
    let etag = header(&current, "etag");
    assert!(!etag.is_empty());
    let first = current.bytes().await.unwrap();

    // Revalidation by ETag: not modified, with the same headers and no body.
    let again = app
        .avatar_raw(&bob, id, Some(1), &[("if-none-match", &etag)])
        .await;
    assert_eq!(again.status().as_u16(), 304);
    assert_eq!(header(&again, "etag"), etag);
    assert_eq!(
        header(&again, "cache-control"),
        "private, max-age=31536000, immutable"
    );
    assert!(again.bytes().await.unwrap().is_empty());
    // A different tag is a full answer.
    let other = app
        .avatar_raw(&bob, id, Some(1), &[("if-none-match", "\"999\"")])
        .await;
    assert_eq!(other.status().as_u16(), 200);

    // A stale or absent version gets the current picture, never cached for good.
    for version in [Some(0), Some(7), None] {
        let response = app.avatar_raw(&bob, id, version, &[]).await;
        assert_eq!(response.status().as_u16(), 200, "{version:?}");
        assert_eq!(header(&response, "cache-control"), "private, no-cache");
        assert_eq!(response.bytes().await.unwrap(), first);
    }
    // A version that is not a number is a bad request.
    let response = app
        .http
        .get(app.url(&format!("/v1/users/{id}/avatar?v=abc")))
        .bearer_auth(&bob.token)
        .send()
        .await
        .unwrap();
    denied(response, ErrorCode::InvalidRequest).await;

    // A new picture is a new version, a new tag, and the old URL no longer says "for good".
    app.set_avatar(&alice, banded_png(120, 120)).await;
    let new = app.avatar_raw(&bob, id, Some(2), &[]).await;
    let new_tag = header(&new, "etag");
    assert_ne!(new_tag, etag);
    assert_eq!(
        header(&new, "cache-control"),
        "private, max-age=31536000, immutable"
    );
    let second = new.bytes().await.unwrap();
    assert_ne!(second, first);
    let stale = app
        .avatar_raw(&bob, id, Some(1), &[("if-none-match", &etag)])
        .await;
    assert_eq!(stale.status().as_u16(), 200);
    assert_eq!(header(&stale, "cache-control"), "private, no-cache");
    assert_eq!(stale.bytes().await.unwrap(), second);
    app.server.shutdown().await.unwrap();
}

// --- Changing and removing -----------------------------------------------------------------------

#[sqlx::test]
async fn a_version_is_never_used_twice_and_the_same_picture_changes_nothing(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let one = png_bytes(64, 64, 255);
    let two = banded_png(120, 120);

    assert_eq!(app.set_avatar(&alice, one.clone()).await.avatar_version, 1);
    let head = app.head_of(app.workspace_id).await;
    // The same picture again: nothing.
    assert_eq!(app.set_avatar(&alice, one.clone()).await.avatar_version, 1);
    assert_eq!(app.head_of(app.workspace_id).await, head);
    assert_eq!(app.set_avatar(&alice, two.clone()).await.avatar_version, 2);
    // Back to the first: a number it has not had.
    assert_eq!(app.set_avatar(&alice, one.clone()).await.avatar_version, 3);

    // Removing: version 0, announced, the picture really gone.
    let response = app.delete_avatar_raw(&alice).await;
    assert_eq!(response.status().as_u16(), 200);
    let removed = response
        .json::<lazy_protocol::ProfileResponse>()
        .await
        .unwrap()
        .user;
    assert_eq!(removed.avatar_version, 0);
    let events = app.profile_events(app.workspace_id, EventSeq::ZERO).await;
    let versions: Vec<u32> = events.iter().map(|e| e.3).collect();
    assert_eq!(versions, [1, 2, 3, 0]);
    let kept: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT png FROM user_avatars WHERE user_id = $1")
            .bind(alice.user.id.as_uuid())
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert!(kept.is_none(), "a removed picture must not be kept");
    for version in [Some(3), Some(0), None] {
        denied(
            app.avatar_raw(&bob, alice.user.id, version, &[]).await,
            ErrorCode::NotFound,
        )
        .await;
    }
    // Removing what is not there: nothing happens, nothing is announced.
    let head = app.head_of(app.workspace_id).await;
    let again = app.delete_avatar_raw(&alice).await;
    assert_eq!(again.status().as_u16(), 200);
    assert_eq!(
        again
            .json::<lazy_protocol::ProfileResponse>()
            .await
            .unwrap()
            .user
            .avatar_version,
        0
    );
    assert_eq!(app.head_of(app.workspace_id).await, head);
    let bob_never = app.delete_avatar_raw(&bob).await;
    assert_eq!(bob_never.status().as_u16(), 200);
    assert_eq!(app.head_of(app.workspace_id).await, head);

    // A picture after the removal is a number beyond everything before, so an old cached address
    // can never show it.
    assert_eq!(app.set_avatar(&alice, two).await.avatar_version, 4);
    assert_eq!(db_profile(&app, alice.user.id).await.1, 4);
    let response = app.avatar_raw(&bob, alice.user.id, Some(3), &[]).await;
    assert_eq!(header(&response, "cache-control"), "private, no-cache");
    let anonymous = app
        .http
        .delete(app.url(lazy_protocol::api::paths::ME_AVATAR_ROUTE))
        .send()
        .await
        .unwrap();
    denied(anonymous, ErrorCode::Unauthenticated).await;
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn an_app_has_no_profile_to_change(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let hook = app
        .create_webhook(&alice, app.general, "Builds")
        .await
        .webhook
        .bot_user_id;
    // An app cannot sign in, so it cannot reach these endpoints; the code refuses it as well.
    let refused = lazy_server::profile::update_profile(
        &app.pool,
        hook,
        lazy_protocol::UpdateProfileRequest {
            display_name: Some(lazy_domain::DisplayName::parse("Renamed").unwrap()),
        },
    )
    .await;
    match refused {
        Err(lazy_server::AppError::Api(error)) => assert_eq!(error.code, ErrorCode::Forbidden),
        other => panic!("expected forbidden, got {other:?}"),
    }
    let refused = lazy_server::profile::remove_avatar(&app.pool, hook).await;
    assert!(matches!(
        refused,
        Err(lazy_server::AppError::Api(ref e)) if e.code == ErrorCode::Forbidden
    ));
    let name: String = sqlx::query_scalar("SELECT display_name FROM users WHERE id = $1")
        .bind(hook.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(name, "Builds");
    app.server.shutdown().await.unwrap();
}

// --- Races ---------------------------------------------------------------------------------------

#[sqlx::test]
async fn profile_changes_racing_workspace_creation_and_joining_leave_every_log_current(
    pool: PgPool,
) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let carol = app.register("carol").await;
    let boot = app.bootstrap_ok(&alice).await;
    let start = (
        profile_of(&boot.workspaces[0], alice.user.id),
        boot.workspaces[0].head,
    );
    // Carol's workspaces, with invitations for alice to use during the race.
    let mut invitations = Vec::new();
    for n in 0..5 {
        let w = app.create_workspace(&carol, &format!("Carol {n}")).await;
        let invite = app
            .invitation_to(&carol, w.workspace.id, serde_json::json!({}))
            .await;
        invitations.push((w.workspace.id, invite.code));
    }

    let renames = async {
        for n in 0..12 {
            if n % 3 == 2 {
                app.set_avatar(&alice, png_bytes(40 + n, 40, 255)).await;
            } else {
                app.rename_me(&alice, &format!("Alice {n}")).await;
            }
        }
    };
    let joins = join_all(invitations.iter().map(|(_, code)| {
        let app = &app;
        let alice = &alice;
        async move { app.accept(alice, code.expose()).await }
    }));
    let creations = join_all((0..4).map(|n| {
        let app = &app;
        let alice = &alice;
        async move { app.create_workspace(alice, &format!("Alice {n}")).await }
    }));
    let ((), joined, created) = tokio::join!(renames, joins, creations);

    // Whatever the interleaving, a client that took each workspace's snapshot when it joined and
    // then applied that workspace's log ends with the person's final profile.
    let final_profile = db_profile(&app, alice.user.id).await;
    assert_eq!(final_profile.0, "Alice 10");
    for workspace in joined.iter().chain(created.iter()) {
        let view = log_view(
            &app,
            workspace.workspace.id,
            alice.user.id,
            profile_of(workspace, alice.user.id),
            workspace.head,
        )
        .await;
        assert_eq!(
            view, final_profile,
            "workspace {}",
            workspace.workspace.name
        );
    }
    let seeded = log_view(&app, app.workspace_id, alice.user.id, start.0, start.1).await;
    assert_eq!(seeded, final_profile);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn people_changing_profiles_while_another_creates_and_joins_workspaces_cannot_deadlock(
    pool: PgPool,
) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let carol = app.register("carol").await;
    // Alice and bob share the seeded workspace and several of carol's, in different join orders.
    let mut shared = Vec::new();
    for n in 0..4 {
        let w = app.create_workspace(&carol, &format!("Shared {n}")).await;
        for person in if n % 2 == 0 {
            [&alice, &bob]
        } else {
            [&bob, &alice]
        } {
            let invite = app
                .invitation_to(&carol, w.workspace.id, serde_json::json!({}))
                .await;
            app.accept(person, invite.code.expose()).await;
        }
        shared.push(w.workspace.id);
    }
    let alice_boot = app.bootstrap_ok(&alice).await;
    let bob_boot = app.bootstrap_ok(&bob).await;

    let rounds = 15;
    let alice_changes = async {
        for n in 0..rounds {
            if n % 4 == 3 {
                app.set_avatar(&alice, png_bytes(30 + n, 30, 255)).await;
            } else {
                app.rename_me(&alice, &format!("Alice {n}")).await;
            }
            // A status too: it reads the same row the profile change writes.
            app.set_status(&alice, &format!("Status {n}"), None).await;
        }
    };
    let bob_changes = async {
        for n in 0..rounds {
            if n % 5 == 4 {
                app.delete_avatar_raw(&bob).await;
            } else if n % 3 == 0 {
                app.set_avatar(&bob, banded_png(100 + n, 100)).await;
            } else {
                app.rename_me(&bob, &format!("Bob {n}")).await;
            }
        }
    };
    let carol_works = async {
        for n in 0..6 {
            let w = app.create_workspace(&carol, &format!("Late {n}")).await;
            for person in [&alice, &bob] {
                let invite = app
                    .invitation_to(&carol, w.workspace.id, serde_json::json!({}))
                    .await;
                app.accept(person, invite.code.expose()).await;
            }
            // And something in an old one, taking its workspace lock.
            app.send(&carol, w.conversations[0].id, "hello").await;
        }
    };
    tokio::join!(alice_changes, bob_changes, carol_works);

    // Every workspace the two are in ends current for both.
    for (person, boot) in [(&alice, &alice_boot), (&bob, &bob_boot)] {
        let final_profile = db_profile(&app, person.user.id).await;
        for workspace in &boot.workspaces {
            let view = log_view(
                &app,
                workspace.workspace.id,
                person.user.id,
                profile_of(workspace, person.user.id),
                workspace.head,
            )
            .await;
            assert_eq!(
                view, final_profile,
                "{} in {}",
                person.user.username, workspace.workspace.name
            );
        }
        let now = app.bootstrap_ok(person).await;
        for workspace in &now.workspaces {
            assert_eq!(
                profile_of(workspace, person.user.id),
                final_profile,
                "bootstrap of {}",
                workspace.workspace.name
            );
        }
    }
    // Each log's positions are still gapless.
    for workspace in shared {
        let positions = app.log_positions_in(workspace).await;
        assert!(
            positions.windows(2).all(|pair| pair[1] == pair[0] + 1),
            "a gap in the log of {workspace}"
        );
    }
    app.server.shutdown().await.unwrap();
}

// --- Locks ---------------------------------------------------------------------------------------

/// Runs `request` while another session holds the person's advisory lock (the one every profile
/// change, workspace creation and invitation acceptance takes first): the request must still be
/// waiting after a bounded time, and must complete once the lock is released.
async fn waits_for_the_person_lock<F>(app: &App, person: UserId, request: F) -> reqwest::Response
where
    F: std::future::Future<Output = reqwest::Response>,
{
    let key = format!("person:{person}");
    let mut held = app.pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
        .bind(&key)
        .execute(&mut *held)
        .await
        .unwrap();
    tokio::pin!(request);
    assert!(
        tokio::time::timeout(Duration::from_millis(500), &mut request)
            .await
            .is_err(),
        "the request did not wait for the person's lock"
    );
    sqlx::query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
        .bind(&key)
        .execute(&mut *held)
        .await
        .unwrap();
    drop(held);
    tokio::time::timeout(DEADLINE, request)
        .await
        .expect("the request finished once the lock was free")
}

#[sqlx::test]
async fn changing_a_profile_or_the_workspaces_one_belongs_to_takes_the_persons_lock(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    let carol = app.register("carol").await;
    let b = app.create_workspace(&carol, "B").await;
    let invite = app
        .invitation_to(&carol, b.workspace.id, serde_json::json!({}))
        .await;
    let id = alice.user.id;

    let response = waits_for_the_person_lock(
        &app,
        id,
        app.patch_me_raw(&alice, serde_json::json!({"display_name": "Alice B"})),
    )
    .await;
    assert!(response.status().is_success());
    let response = waits_for_the_person_lock(
        &app,
        id,
        app.set_avatar_raw(&alice, png_bytes(32, 32, 255), "image/png"),
    )
    .await;
    assert!(response.status().is_success());
    let response = waits_for_the_person_lock(&app, id, app.delete_avatar_raw(&alice)).await;
    assert!(response.status().is_success());
    let response = waits_for_the_person_lock(
        &app,
        id,
        app.create_workspace_raw(&alice, WorkspaceId::generate(), "Mine"),
    )
    .await;
    assert!(response.status().is_success());
    let response =
        waits_for_the_person_lock(&app, id, app.accept_raw(&alice, invite.code.expose())).await;
    assert!(response.status().is_success());
    // Someone else's lock is not hers: carol is not held up by a lock on alice.
    assert!(app.rename_me(&carol, "Carol C").await.display_name.as_str() == "Carol C");
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn setting_a_status_waits_for_a_profile_change_that_is_writing_the_same_person(pool: PgPool) {
    let app = App::start(pool).await;
    let alice = app.register("alice").await;
    // A profile change holds the person's row like this while it announces itself.
    let mut tx = app.pool.begin().await.unwrap();
    sqlx::query("UPDATE users SET display_name = 'Alice Changing' WHERE id = $1")
        .bind(alice.user.id.as_uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
    let response = {
        let status = app.set_status_raw(
            &alice,
            app.workspace_id,
            serde_json::json!({"status": {"text": "Busy"}}),
        );
        tokio::pin!(status);
        assert!(
            tokio::time::timeout(Duration::from_millis(500), &mut status)
                .await
                .is_err(),
            "a status did not wait for the profile change"
        );
        tx.commit().await.unwrap();
        tokio::time::timeout(DEADLINE, status).await.unwrap()
    };
    assert!(response.status().is_success());
    // And its answer carries the profile as the change left it.
    let user = response
        .json::<lazy_protocol::StatusResponse>()
        .await
        .unwrap()
        .user;
    assert_eq!(user.display_name.as_str(), "Alice Changing");
    app.server.shutdown().await.unwrap();
}
